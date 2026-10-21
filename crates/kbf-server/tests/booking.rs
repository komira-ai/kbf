//! Sized bookings end to end: `kbf-book-cpus` and `kbf-book-mem-gib` on an action, the
//! booking in the daemon's `Start`, and placement that books its cores on the node (its
//! memory is the limit the action runs under, not a share of the node's).

mod support;

use support::{Job, done, output, ran};

const GIB: u64 = 1 << 30;

/// Catches: a sized booking that reaches the scheduler but not the daemon's `Start`
/// (the native driver sets its memory kill from `Start.memory_bytes`, so a large link
/// step would still die at 2 GiB); cores the scheduler does not hold on the node, so a
/// second large action is placed beside the first on cores the node does not have; a
/// memory limit held against the node's memory, so an action whose limit does not fit
/// beside the first's is kept waiting although a core is free; and cores kept after
/// their lease ends, so the queued action never runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sized_booking_reaches_the_daemon_and_holds_the_node() {
    let cell = support::Cell::start().await;
    let mut node = cell.daemon("node-a", 8, 16).await;
    let big = [("kbf-book-cpus", "5"), ("kbf-book-mem-gib", "12")];
    let first = Job::new("link one", &big);
    let second = Job::new("link two", &big);
    let small = Job::new("compile", &[("kbf-book-mem-gib", "8")]);
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
        (5_000, 12 * GIB)
    );

    // 3 cores are left: the second 5-core booking waits, a 1-core action does not, though
    // its 8 GiB limit and the first's 12 GiB add up to more than the node's 16 GiB.
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
        (1_000, 8 * GIB)
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
