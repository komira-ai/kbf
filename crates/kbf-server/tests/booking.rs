//! Sized bookings end to end: `kbf-book-cpus` and `kbf-book-mem-gib` on an action, the
//! booking in the daemon's `Start`, and placement that books it on the node.

mod support;

use support::{Job, done, output, ran};

const GIB: u64 = 1 << 30;

/// Catches: a sized booking that reaches the scheduler but not the daemon's `Start`
/// (the native driver sets its memory kill from `Start.memory_bytes`, so a large link
/// step would still die at 2 GiB); a booking the scheduler does not hold on the node,
/// so a second large action is placed beside the first in memory the node does not
/// have; and a booking kept after its lease ends, so the queued action never runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sized_booking_reaches_the_daemon_and_holds_the_node() {
    let cell = support::Cell::start().await;
    let mut node = cell.daemon("node-a", 8, 16).await;
    let big = [("kbf-book-cpus", "4"), ("kbf-book-mem-gib", "12")];
    let first = Job::new("link one", &big);
    let second = Job::new("link two", &big);
    let small = Job::new("compile", &[]);
    for job in [&first, &second, &small] {
        cell.upload(&job.blobs()).await;
    }

    let mut first_ops = cell.execute(&first.action).await;
    let first_start = node.start().await;
    assert_eq!(
        first_start.action_digest.as_ref(),
        Some(&first.action.proto)
    );
    assert_eq!(
        (first_start.millicpus, first_start.memory_bytes),
        (4_000, 12 * GIB)
    );

    // 4 GiB are left: the second 12 GiB booking waits, a 1 GiB action does not.
    let mut second_ops = cell.execute(&second.action).await;
    node.no_work().await;
    let mut small_ops = cell.execute(&small.action).await;
    let small_start = node.start().await;
    assert_eq!(
        small_start.action_digest.as_ref(),
        Some(&small.action.proto)
    );
    assert_eq!(
        (small_start.millicpus, small_start.memory_bytes),
        (1_000, GIB)
    );
    let compiled = output(&cell, "compiled", 0).await;
    assert!(
        node.report(ran(small_start.lease_id, &compiled))
            .await
            .accepted
    );
    done(&mut small_ops).await;

    let linked = output(&cell, "linked once", 0).await;
    assert!(
        node.report(ran(first_start.lease_id, &linked))
            .await
            .accepted
    );
    done(&mut first_ops).await;
    let second_start = node.start().await;
    assert_eq!(
        second_start.action_digest.as_ref(),
        Some(&second.action.proto)
    );
    assert_eq!(second_start.memory_bytes, 12 * GIB);
    let linked = output(&cell, "linked twice", 0).await;
    assert!(
        node.report(ran(second_start.lease_id, &linked))
            .await
            .accepted
    );
    done(&mut second_ops).await;
}

/// Catches: `--book-mem-gib-by-category` not applied, so a `mojo_build_test` action
/// still books 1 GiB and is killed at its cgroup cap (the 1 GiB it booked) before the
/// ladder raises it; a category entry booked past the largest node that could run the
/// action (the 64 GiB entry would wait on a 16 GiB node and then fail instead of
/// running at 16); a category entry overriding a `kbf-book-mem-gib` the action named;
/// and a category the table does not name booking anything but the 1 GiB default.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_category_books_its_entry_up_to_the_largest_node() {
    let table = "mojo_build_test=4,huge=64".parse().expect("a table");
    let cell = support::Cell::start_booking_by_category(table).await;
    let mut node = cell.daemon("node-a", 8, 16).await;
    let tag = kbf_front::CATEGORY_ENV;
    let cases = [
        ("build test", vec![], "mojo_build_test", 4 * GIB),
        ("huge", vec![], "huge", 16 * GIB),
        (
            "sized",
            vec![("kbf-book-mem-gib", "2")],
            "mojo_build_test",
            2 * GIB,
        ),
        ("not in the table", vec![], "c_compile", GIB),
    ];
    for (argv, platform, category, memory) in cases {
        let job = Job::with_env(argv, &platform, &[(tag, category)]);
        cell.upload(&job.blobs()).await;
        let mut ops = cell.execute(&job.action).await;
        let start = node.start().await;
        assert_eq!(start.action_digest.as_ref(), Some(&job.action.proto));
        assert_eq!(start.memory_bytes, memory, "{argv}");
        let result = output(&cell, argv, 0).await;
        assert!(node.report(ran(start.lease_id, &result)).await.accepted);
        done(&mut ops).await;
    }
}

/// Catches: `--book-cpus-by-category` not applied, so a `mojo_build_test` action still
/// books one core (and a node of 8 cores runs 8 of them at once); an entry booked past
/// the largest node that could run the action (the 64-core entry would wait on an
/// 8-core node and fail instead of running at 8); an entry overriding a
/// `kbf-book-cpus` the action named; an entry dropped because the action named only
/// its memory (the two bookings are independent); and a category the table does not
/// name, or an action with none, booking anything but one core.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cpu_category_books_its_cores_up_to_the_largest_node() {
    let table = "mojo_build_test=4,huge=64".parse().expect("a table");
    let cell = support::Cell::start_booking_cpus_by_category(table).await;
    let mut node = cell.daemon("node-a", 8, 16).await;
    let tag = kbf_front::CATEGORY_ENV;
    let cases = [
        ("build test", vec![], Some("mojo_build_test"), 4_000, GIB),
        ("huge", vec![], Some("huge"), 8_000, GIB),
        (
            "sized cores",
            vec![("kbf-book-cpus", "2")],
            Some("mojo_build_test"),
            2_000,
            GIB,
        ),
        (
            "sized memory",
            vec![("kbf-book-mem-gib", "2")],
            Some("mojo_build_test"),
            4_000,
            2 * GIB,
        ),
        ("not in the table", vec![], Some("c_compile"), 1_000, GIB),
        ("untagged", vec![], None, 1_000, GIB),
    ];
    for (argv, platform, category, cpus, memory) in cases {
        let env: Vec<(&str, &str)> = category.map(|c| (tag, c)).into_iter().collect();
        let job = Job::with_env(argv, &platform, &env);
        cell.upload(&job.blobs()).await;
        let mut ops = cell.execute(&job.action).await;
        let start = node.start().await;
        assert_eq!(start.action_digest.as_ref(), Some(&job.action.proto));
        assert_eq!(
            (start.millicpus, start.memory_bytes),
            (cpus, memory),
            "{argv}"
        );
        let result = output(&cell, argv, 0).await;
        assert!(node.report(ran(start.lease_id, &result)).await.accepted);
        done(&mut ops).await;
    }
}

/// Catches: the cores a category books not held on the node, so a node of 8 cores runs
/// a third 4-core action beside two; a held booking not released when its lease ends,
/// so the third never runs; and a 1-core action of no category starting while the 8
/// cores are booked. This is the cap `--book-cpus-by-category` puts on how many such
/// actions are open on a node at once: 8 cores / 4 = 2.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cpu_category_caps_the_leases_open_at_once_on_a_node() {
    let table = "mojo_build_test=4".parse().expect("a table");
    let cell = support::Cell::start_booking_cpus_by_category(table).await;
    let mut node = cell.daemon("node-a", 8, 16).await;
    let tag = kbf_front::CATEGORY_ENV;
    let heavy = |argv| Job::with_env(argv, &[], &[(tag, "mojo_build_test")]);
    let jobs = [heavy("build one"), heavy("build two"), heavy("build three")];
    let light = Job::new("compile", &[]);
    for job in jobs.iter().chain([&light]) {
        cell.upload(&job.blobs()).await;
    }
    let mut ops = Vec::new();
    for job in &jobs {
        ops.push(cell.execute(&job.action).await);
    }
    let first = node.start().await;
    let second = node.start().await;
    assert_eq!((first.millicpus, second.millicpus), (4_000, 4_000));
    // All 8 cores are booked: the third heavy action waits, and so does a light one.
    node.no_work().await;

    let built = output(&cell, "built", 0).await;
    assert!(node.report(ran(first.lease_id, &built)).await.accepted);
    done(&mut ops[0]).await;
    let third = node.start().await;
    assert_eq!(third.action_digest.as_ref(), Some(&jobs[2].action.proto));
    assert_eq!(third.millicpus, 4_000);

    // The third and the second hold all 8 cores again: a light action waits too.
    let mut light_ops = cell.execute(&light.action).await;
    node.no_work().await;
    assert!(node.report(ran(second.lease_id, &built)).await.accepted);
    done(&mut ops[1]).await;
    let small = node.start().await;
    assert_eq!(small.action_digest.as_ref(), Some(&light.action.proto));
    assert_eq!(small.millicpus, 1_000);
    for (start, stream) in [(third, &mut ops[2]), (small, &mut light_ops)] {
        assert!(node.report(ran(start.lease_id, &built)).await.accepted);
        done(stream).await;
    }
}
