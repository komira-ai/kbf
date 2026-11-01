//! What Execute reads from the platform: the lease kind, the GPU count, the booking,
//! which workers may run the action, and every property name in any case.

use kbf_front::{BOOK_CPUS_KEY, BOOK_MEM_GIB_KEY, GPU_KEY};
use kbf_types::Resources;

use super::*;

/// Catches: a lease kind read from the wrong place (REAPI 2.2 clients put the platform
/// in the Action, older ones in the Command), including an Action whose platform is
/// present but empty hiding the Command's; an unknown kind run as a shared action; and
/// a platform with a repeated property accepted.
#[tokio::test]
async fn the_lease_kind_comes_from_the_platform() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;

    let in_action = job("mac", &[("kbf-lease", "whole_machine")], true);
    farm.upload(&in_action.blobs.iter().collect::<Vec<_>>())
        .await;
    start(&farm, &in_action.action).await.expect("Execute");

    // An older client: the Action has no platform, the Command has one.
    let older = job("old", &[], false);
    #[allow(deprecated)]
    let command = Blob::of(&Command {
        arguments: vec!["old".to_owned()],
        platform: Some(Platform {
            properties: vec![property("kbf-lease", "whole_machine")],
        }),
        ..Default::default()
    });
    let action = Blob::of(&Action {
        command_digest: Some(command.proto.clone()),
        input_root_digest: Some(older.blobs[2].proto.clone()),
        ..Default::default()
    });
    farm.upload(&[&action, &command, &older.blobs[2], &older.blobs[3]])
        .await;
    start(&farm, &action).await.expect("Execute");
    // The same, but the Action carries an empty platform rather than none.
    #[allow(deprecated)]
    let command = Blob::of(&Command {
        arguments: vec!["old, empty platform".to_owned()],
        platform: Some(Platform {
            properties: vec![property("kbf-lease", "whole_machine")],
        }),
        ..Default::default()
    });
    let action = Blob::of(&Action {
        command_digest: Some(command.proto.clone()),
        input_root_digest: Some(older.blobs[2].proto.clone()),
        platform: Some(Platform::default()),
        ..Default::default()
    });
    farm.upload(&[&action, &command]).await;
    start(&farm, &action).await.expect("Execute");

    let submitted = script.submitted();
    assert_eq!(submitted[0].request.kind, LeaseKind::WholeMachine);
    assert!(submitted[0].request.do_not_cache, "do_not_cache dropped");
    assert!(!submitted[0].request.joinable());
    assert_eq!(
        submitted[1].request.kind,
        LeaseKind::WholeMachine,
        "the Command's platform ignored"
    );
    assert_eq!(
        submitted[2].request.kind,
        LeaseKind::WholeMachine,
        "an empty Action platform hid the Command's"
    );

    for (why, props) in [
        ("an unknown kind", vec![("kbf-lease", "vm")]),
        (
            "a repeated property",
            vec![("os", "linux"), ("os", "linux")],
        ),
    ] {
        let bad = job(why, &props, false);
        farm.upload(&bad.blobs.iter().collect::<Vec<_>>()).await;
        let status = start(&farm, &bad.action).await.expect_err(why);
        assert_eq!(status.code(), Code::InvalidArgument, "{why}");
    }
    assert_eq!(script.submitted().len(), 3);
}

/// Catches: a `gpu` property dropped on the way to the scheduler (a GPU action would
/// be placed on a node without one), GPUs booked for an action that asked for none,
/// and a `gpu` value that is not a count accepted.
#[tokio::test]
async fn the_gpu_count_comes_from_the_platform() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    for (argv, props) in [
        ("two gpus", vec![(GPU_KEY, "2")]),
        ("no gpu", vec![("kbf-lease", "action")]),
    ] {
        let job = job(argv, &props, false);
        farm.upload(&job.blobs.iter().collect::<Vec<_>>()).await;
        start(&farm, &job.action).await.expect("Execute");
    }
    let submitted = script.submitted();
    assert_eq!(
        submitted[0].request.resources,
        DEFAULT_RESOURCES.with_gpus(2)
    );
    assert_eq!(submitted[1].request.resources, DEFAULT_RESOURCES);
    assert_eq!(DEFAULT_RESOURCES.gpus, 0);

    for value in ["one", "-1", ""] {
        let bad = job(&format!("gpu={value}"), &[(GPU_KEY, value)], false);
        farm.upload(&bad.blobs.iter().collect::<Vec<_>>()).await;
        let status = start(&farm, &bad.action).await.expect_err(value);
        assert_eq!(status.code(), Code::InvalidArgument, "{value:?}");
    }
    assert_eq!(script.submitted().len(), 2);
}

/// Catches: `kbf-book-cpus` or `kbf-book-mem-gib` dropped on the way to the scheduler
/// (a large link step books 1 GiB and the native driver kills it at 2 GiB), one key
/// replacing the other's default, a key read in its exact spelling only, a booking of
/// zero or one that overflows accepted, a second spelling of a size (`+4`, `04`: the
/// same booking, another cache entry) accepted, and either key accepted on a
/// `whole_machine` lease.
#[tokio::test]
async fn the_booking_comes_from_the_platform() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    type Case = (&'static str, Vec<(&'static str, &'static str)>, u64, u64);
    let cases: [Case; 4] = [
        (
            "both",
            vec![
                (BOOK_CPUS_KEY, "6"),
                (BOOK_MEM_GIB_KEY, "17"),
                (GPU_KEY, "1"),
            ],
            6_000,
            17 << 30,
        ),
        ("cpus only", vec![(BOOK_CPUS_KEY, "4")], 4_000, 4 << 30),
        (
            "memory only",
            vec![("KBF-Book-Mem-GiB", "8")],
            1_000,
            8 << 30,
        ),
        ("neither", vec![("kbf-lease", "action")], 1_000, 4 << 30),
    ];
    for (argv, props, _, _) in &cases {
        let job = job(argv, props, false);
        farm.upload(&job.blobs.iter().collect::<Vec<_>>()).await;
        start(&farm, &job.action).await.expect(argv);
    }
    let submitted = script.submitted();
    for (submission, (argv, props, cpu_millis, memory_bytes)) in submitted.iter().zip(&cases) {
        let gpus = u64::from(props.contains(&(GPU_KEY, "1")));
        assert_eq!(
            submission.request.resources,
            kbf_types::Resources::new(*cpu_millis, *memory_bytes).with_gpus(gpus),
            "{argv}"
        );
        assert_eq!(
            submission.request.needs,
            kbf_caps::Request::default(),
            "{argv}"
        );
    }

    let too_many = (u64::MAX / 1_000 + 1).to_string();
    let too_much = (u64::MAX >> 30).saturating_add(1).to_string();
    for (why, props) in [
        ("zero cores", vec![(BOOK_CPUS_KEY, "0")]),
        ("no memory", vec![(BOOK_MEM_GIB_KEY, "0")]),
        ("half a core", vec![(BOOK_CPUS_KEY, "0.5")]),
        ("a negative size", vec![(BOOK_MEM_GIB_KEY, "-1")]),
        ("a word", vec![(BOOK_MEM_GIB_KEY, "lots")]),
        ("a plus sign", vec![(BOOK_CPUS_KEY, "+4")]),
        ("a plus sign on memory", vec![(BOOK_MEM_GIB_KEY, "+4")]),
        ("a leading zero", vec![(BOOK_CPUS_KEY, "04")]),
        ("a leading zero on memory", vec![(BOOK_MEM_GIB_KEY, "016")]),
        ("zero spelled long", vec![(BOOK_CPUS_KEY, "00")]),
        ("a space", vec![(BOOK_CPUS_KEY, " 4")]),
        ("nothing", vec![(BOOK_MEM_GIB_KEY, "")]),
        ("too many cores", vec![(BOOK_CPUS_KEY, too_many.as_str())]),
        (
            "too much memory",
            vec![(BOOK_MEM_GIB_KEY, too_much.as_str())],
        ),
        (
            "cores on a whole machine",
            vec![("kbf-lease", "whole_machine"), (BOOK_CPUS_KEY, "4")],
        ),
        (
            "memory on a whole machine",
            vec![(BOOK_MEM_GIB_KEY, "4"), ("kbf-lease", "whole_machine")],
        ),
        (
            "two spellings",
            vec![(BOOK_CPUS_KEY, "4"), ("KBF-BOOK-CPUS", "4")],
        ),
    ] {
        let bad = job(why, &props, false);
        farm.upload(&bad.blobs.iter().collect::<Vec<_>>()).await;
        let status = start(&farm, &bad.action).await.expect_err(why);
        assert_eq!(status.code(), Code::InvalidArgument, "{why}: {status:?}");
        assert!(status.message().contains("kbf-book-"), "{why}: {status:?}");
    }
    assert_eq!(script.submitted().len(), cases.len());
}

/// Catches: the platform's OS and architecture dropped on the way to the scheduler (a
/// Linux action could then run on a Mac), properties that are not capabilities
/// refused, a malformed platform accepted, and a platform no kbf daemon can ever run
/// queued to wait out the bound instead of refused at once.
#[tokio::test]
async fn the_platform_says_which_workers_may_run_it() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let image = ("container-image", "docker://img@sha256:00");
    type Props = Vec<(&'static str, &'static str)>;
    let cases: [(&str, Props, Props); 3] = [
        (
            "linux",
            vec![("OSFamily", "Linux"), image],
            vec![("os", "linux")],
        ),
        (
            "mac",
            vec![("OSFamily", "darwin"), ("ISA", "arm-a64")],
            vec![("os", "macos"), ("arch", "arm64")],
        ),
        ("anywhere", vec![], vec![]),
    ];
    for (argv, props, _) in &cases {
        let job = job(argv, props, false);
        farm.upload(&job.blobs.iter().collect::<Vec<_>>()).await;
        start(&farm, &job.action).await.expect("Execute");
    }
    for (submission, (argv, _, needs)) in script.submitted().iter().zip(&cases) {
        let want = kbf_caps::Request::parse(needs.iter().copied()).unwrap();
        assert_eq!(submission.request.needs, want, "{argv}");
    }

    for (why, props, code) in [
        (
            "one requirement twice",
            vec![("OSFamily", "linux"), ("os", "linux")],
            Code::InvalidArgument,
        ),
        (
            "a count that is not one",
            vec![("cpus", "many")],
            Code::InvalidArgument,
        ),
        (
            "an OS no daemon runs",
            vec![("OSFamily", "Windows")],
            Code::FailedPrecondition,
        ),
        (
            "an ISA no daemon runs",
            vec![("ISA", "x86-32")],
            Code::FailedPrecondition,
        ),
    ] {
        let bad = job(why, &props, false);
        farm.upload(&bad.blobs.iter().collect::<Vec<_>>()).await;
        let status = start(&farm, &bad.action).await.expect_err(why);
        assert_eq!(status.code(), code, "{why}: {status:?}");
        assert!(status.details().is_empty(), "{why}: nothing to upload");
    }
    assert_eq!(script.submitted().len(), 3);
}

/// Catches: a property kbf reads ignored because of its case, so `osfamily=linux`
/// runs anywhere, `GPU=1` on a node without a GPU, `KBF-LEASE=whole_machine` beside
/// other work; and one property sent in two spellings resolved silently by order.
/// (On macOS a GPU is the Mac's one slot; see `a_gpu_on_macos_is_the_macs_one_slot`.)
#[tokio::test]
async fn property_names_are_read_in_any_case() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let props = [
        ("osfamily", "linux"),
        ("isa", "arm-a64"),
        ("GPU", "1"),
        ("KBF-Lease", "whole_machine"),
        ("Pool", "default"),
    ];
    let any_case = job("any case", &props, false);
    farm.upload(&any_case.blobs.iter().collect::<Vec<_>>())
        .await;
    start(&farm, &any_case.action).await.expect("Execute");
    let [submitted] = script.submitted().try_into().expect("one submission");
    let want = kbf_caps::Request::parse([("os", "linux"), ("arch", "arm64")]).unwrap();
    assert_eq!(submitted.request.needs, want);
    assert_eq!(
        submitted.request.resources,
        Resources::new(1_000, 1 << 30).with_gpus(1)
    );
    assert_eq!(submitted.request.kind, LeaseKind::WholeMachine);

    for props in [
        vec![("gpu", "1"), ("GPU", "2")],
        vec![("kbf-lease", "action"), ("Kbf-Lease", "whole_machine")],
        vec![("OSFamily", "linux"), ("osfamily", "darwin")],
    ] {
        let why = format!("{props:?}");
        let bad = job(&why, &props, false);
        farm.upload(&bad.blobs.iter().collect::<Vec<_>>()).await;
        let status = start(&farm, &bad.action).await.expect_err(&why);
        assert_eq!(status.code(), Code::InvalidArgument, "{why}");
        assert!(status.message().contains("two spellings"), "{status:?}");
    }
    assert_eq!(script.submitted().len(), 1);
}

/// Catches: an `ios.device` request still refused once devices are booked, or passed on
/// without its device attributes (the scheduler would book any device); and each
/// device refusal the design names let through to the scheduler: a device on a
/// `whole_machine` lease, an attribute without `ios.device=1`, an attribute kbf does
/// not know, a count other than 1, and a device on Linux. Also catches names that only
/// begin like a device key (`ios.devices`, `ios.simulator`, `iosx`) refused instead of
/// left alone.
#[tokio::test]
async fn ios_device_requests_reach_the_scheduler_and_bad_ones_are_refused() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let good = [
        ("OSFamily", "darwin"),
        ("IOS.Device", "1"),
        ("ios.device.class", "iPad"),
    ];
    let device = job("device", &good, false);
    farm.upload(&device.blobs.iter().collect::<Vec<_>>()).await;
    start(&farm, &device.action)
        .await
        .expect("a device request");
    let submitted = script.submitted();
    let [submission] = submitted.as_slice() else {
        panic!("{submitted:?}");
    };
    let canonical = [
        ("OSFamily", "darwin"),
        ("ios.device", "1"),
        ("ios.device.class", "iPad"),
    ];
    assert_eq!(
        submission.request.needs,
        kbf_caps::Request::from_platform(canonical).unwrap()
    );
    assert!(submission.request.needs.wants_device());

    for (why, props, names) in [
        (
            "whole machine",
            vec![("kbf-lease", "whole_machine"), ("ios.device", "1")],
            "whole_machine",
        ),
        (
            "alone",
            vec![("ios.device.os_version", "26.0")],
            "\"ios.device.os_version\"",
        ),
        (
            "unknown",
            vec![("ios.device", "1"), ("ios.device.os", "26.0")],
            "\"ios.device.os\"",
        ),
        ("count", vec![("ios.device", "2")], "\"ios.device\""),
        (
            "linux",
            vec![("OSFamily", "linux"), ("ios.device", "1")],
            "\"linux\"",
        ),
    ] {
        let bad = job(why, &props, false);
        farm.upload(&bad.blobs.iter().collect::<Vec<_>>()).await;
        let status = start(&farm, &bad.action).await.expect_err(why);
        assert_eq!(status.code(), Code::InvalidArgument, "{why}: {status:?}");
        assert!(status.message().contains(names), "{why}: {status:?}");
    }
    assert_eq!(script.submitted().len(), 1);

    for name in ["ios.devices", "ios.simulator", "iosx"] {
        let other = job(name, &[(name, "1")], false);
        farm.upload(&other.blobs.iter().collect::<Vec<_>>()).await;
        start(&farm, &other.action).await.expect(name);
    }
    let submitted = script.submitted();
    assert_eq!(submitted.len(), 4);
    for submission in &submitted[1..] {
        assert_eq!(submission.request.needs, kbf_caps::Request::default());
    }
}

/// Catches a GPU on macOS booked on top of a whole-machine lease (the lease has the
/// whole Mac, so a booked GPU slot would wait behind other GPU work for a GPU it
/// already holds); an `action` lease for a GPU on macOS refused (the Mac's one GPU slot
/// is bookable, so a GPU test runs beside the Mac's other work) or booked as none (two
/// GPU tests would share the GPU); more than the Mac's one GPU accepted (no node could
/// serve it and it would wait for ever; it is refused, naming the one GPU); and the same
/// request for Linux booked differently (`gpu=2` stays a booking of two); two GPUs on a
/// `whole_machine` lease on macOS accepted (it is refused like the action lease); and a
/// request that names no OS booked as a Mac's (`arch` and `gpu=1` book one GPU as asked). An `os`
/// property names macOS as `OSFamily` does.
#[tokio::test]
async fn a_gpu_on_macos_is_the_macs_one_slot() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    for os in [("OSFamily", "darwin"), ("os", "macos")] {
        let whole = job(
            &format!("whole {os:?}"),
            &[os, ("gpu", "1"), ("kbf-lease", "whole_machine")],
            false,
        );
        farm.upload(&whole.blobs.iter().collect::<Vec<_>>()).await;
        start(&farm, &whole.action).await.expect("Execute");
        let shared = job(&format!("action {os:?}"), &[os, ("gpu", "1")], false);
        farm.upload(&shared.blobs.iter().collect::<Vec<_>>()).await;
        start(&farm, &shared.action).await.expect("Execute");
        let two = job(&format!("two {os:?}"), &[os, ("gpu", "2")], false);
        farm.upload(&two.blobs.iter().collect::<Vec<_>>()).await;
        let status = start(&farm, &two.action).await.expect_err("two GPUs");
        assert_eq!(status.code(), Code::InvalidArgument);
        assert!(status.message().contains("one GPU"), "{status:?}");
        // The whole Mac has one GPU too: two is refused for that lease kind as well.
        let whole_two = job(
            &format!("whole two {os:?}"),
            &[os, ("gpu", "2"), ("kbf-lease", "whole_machine")],
            false,
        );
        farm.upload(&whole_two.blobs.iter().collect::<Vec<_>>())
            .await;
        let status = start(&farm, &whole_two.action).await.expect_err("two GPUs");
        assert_eq!(status.code(), Code::InvalidArgument);
        assert!(status.message().contains("one GPU"), "{status:?}");
    }
    let linux = job("linux", &[("os", "linux"), ("gpu", "2")], false);
    farm.upload(&linux.blobs.iter().collect::<Vec<_>>()).await;
    start(&farm, &linux.action).await.expect("Execute");
    // No OS named: not a request for a Mac, so booked as asked.
    let no_os = job("no os", &[("arch", "arm64"), ("gpu", "1")], false);
    farm.upload(&no_os.blobs.iter().collect::<Vec<_>>()).await;
    start(&farm, &no_os.action).await.expect("Execute");
    let submitted = script.submitted();
    assert_eq!(submitted.len(), 6);
    let booked = |i: usize| {
        (
            submitted[i].request.kind,
            submitted[i].request.resources.gpus,
        )
    };
    // Per spelling: the whole-machine lease, then the action lease.
    assert_eq!(booked(0), (LeaseKind::WholeMachine, 0));
    assert_eq!(booked(1), (LeaseKind::Action, 1));
    assert_eq!(booked(2), (LeaseKind::WholeMachine, 0));
    assert_eq!(booked(3), (LeaseKind::Action, 1));
    assert_eq!(booked(4), (LeaseKind::Action, 2));
    assert_eq!(booked(5), (LeaseKind::Action, 1));
    for i in [0, 2] {
        assert_eq!(
            submitted[i].request.resources,
            Resources::new(1_000, 1 << 30),
            "a whole_machine lease's least worker is not the action default"
        );
    }
}

/// Catches: an action that names no memory size booking anything but 4 GiB (the first
/// rung of the default ladder); a named size replaced by the default; and a
/// `whole_machine` lease, which books the whole worker, asking for the 4 GiB a worker
/// smaller than that could never give.
#[tokio::test]
async fn an_action_that_names_no_memory_books_the_first_rung() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let unsized_ = job("unsized", &[], false);
    let named = job("named", &[(BOOK_MEM_GIB_KEY, "1")], false);
    let whole = job("whole", &[("kbf-lease", "whole_machine")], false);
    for j in [&unsized_, &named, &whole] {
        farm.upload(&j.blobs.iter().collect::<Vec<_>>()).await;
        start(&farm, &j.action).await.expect("Execute");
    }
    let submitted = script.submitted();
    assert_eq!(submitted[0].request.resources.memory_bytes, 4 << 30);
    assert_eq!(submitted[0].request.resources.cpu_millis, 1_000);
    assert_eq!(submitted[1].request.resources.memory_bytes, 1 << 30);
    assert_eq!(submitted[2].request.resources.memory_bytes, 1 << 30);
}

/// Catches a first rung that never reaches the service (the server's
/// `--memory-ladder-gib` ignored): with 2 GiB set, an unsized action books 2 GiB, a
/// named size still books as named, and a `whole_machine` lease is untouched.
#[tokio::test]
async fn the_default_memory_is_the_one_the_service_is_given() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_default_memory(Arc::clone(&script), 2 << 30).await;
    let unsized_ = job("unsized", &[], false);
    let named = job("named", &[(BOOK_MEM_GIB_KEY, "8")], false);
    let whole = job("whole", &[("kbf-lease", "whole_machine")], false);
    for j in [&unsized_, &named, &whole] {
        farm.upload(&j.blobs.iter().collect::<Vec<_>>()).await;
        start(&farm, &j.action).await.expect("Execute");
    }
    let submitted = script.submitted();
    assert_eq!(submitted[0].request.resources.memory_bytes, 2 << 30);
    assert_eq!(submitted[1].request.resources.memory_bytes, 8 << 30);
    assert_eq!(submitted[2].request.resources.memory_bytes, 1 << 30);
}

/// Catches the exclusive-actions setting reaching the scheduler wrongly: off, no request
/// owns a worker; on, an `action` does whatever it names (a named size is still read,
/// checked and booked as the hint it is) and its booking and sources are as they were, but
/// a `whole_machine` lease is not asked for as a whole worker, and a malformed size is
/// refused as it is with the setting off.
#[tokio::test]
async fn exclusive_actions_ask_the_scheduler_for_a_whole_worker() {
    use kbf_invocation::BookSource::{Default as Unnamed, Hint};
    let jobs = [
        job("plain", &[], false),
        job(
            "named",
            &[(BOOK_CPUS_KEY, "2"), (BOOK_MEM_GIB_KEY, "8")],
            false,
        ),
        job("whole", &[("kbf-lease", "whole_machine")], false),
    ];
    let mut seen = Vec::new();
    for exclusive in [false, true] {
        let script = Arc::new(Script::default());
        let farm = if exclusive {
            Farm::with_exclusive_actions(Arc::clone(&script)).await
        } else {
            Farm::with_execution(Arc::clone(&script)).await
        };
        for j in &jobs {
            farm.upload(&j.blobs.iter().collect::<Vec<_>>()).await;
            start(&farm, &j.action).await.expect("Execute");
        }
        let submitted = script.submitted();
        let whole: Vec<bool> = submitted.iter().map(|s| s.request.whole_worker).collect();
        let booked: Vec<_> = submitted.iter().map(|s| s.request.resources).collect();
        let sources: Vec<_> = submitted
            .iter()
            .map(|s| (s.evidence.cpu_source, s.evidence.memory_source))
            .collect();
        assert_eq!(
            whole,
            [exclusive, exclusive, false],
            "exclusive={exclusive}"
        );
        assert_eq!(
            sources,
            [(Unnamed, Unnamed), (Hint, Hint), (Unnamed, Unnamed)]
        );
        seen.push(booked);
    }
    // The booking the scheduler would otherwise use is the same either way.
    assert_eq!(seen[0], seen[1]);
}

/// Catches: a named size that equals the default read as the default (the estimator
/// would then lower an explicit client ask: the source decided by comparing the
/// booked value to the default), one axis's key marking the other's source, a source
/// recorded for the wrong axis, and a `whole_machine` lease, which names nothing,
/// reading as a hint.
#[tokio::test]
async fn the_booking_source_is_recorded_per_axis() {
    use kbf_invocation::BookSource::{Default as Unnamed, Hint};
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let cases = [
        ("neither", vec![], Unnamed, Unnamed),
        (
            "memory equal to the default",
            vec![(BOOK_MEM_GIB_KEY, "4")],
            Unnamed,
            Hint,
        ),
        (
            "cpus equal to the default",
            vec![(BOOK_CPUS_KEY, "1")],
            Hint,
            Unnamed,
        ),
        ("cpus only", vec![(BOOK_CPUS_KEY, "4")], Hint, Unnamed),
        (
            "both",
            vec![(BOOK_CPUS_KEY, "2"), (BOOK_MEM_GIB_KEY, "9")],
            Hint,
            Hint,
        ),
        (
            "a whole machine",
            vec![("kbf-lease", "whole_machine")],
            Unnamed,
            Unnamed,
        ),
    ];
    for (argv, props, _, _) in &cases {
        let job = job(argv, props, false);
        farm.upload(&job.blobs.iter().collect::<Vec<_>>()).await;
        start(&farm, &job.action).await.expect(argv);
    }
    let submitted = script.submitted();
    assert_eq!(submitted.len(), cases.len());
    for (submission, (argv, _, cpu, memory)) in submitted.iter().zip(&cases) {
        assert_eq!(submission.evidence.cpu_source, *cpu, "{argv}: cpu");
        assert_eq!(submission.evidence.memory_source, *memory, "{argv}: memory");
        assert_eq!(
            (
                submission.evidence.input_bytes,
                submission.evidence.input_files
            ),
            (None, None),
            "{argv}: the in-process cache's walk totals nothing"
        );
    }
}

/// The `needs` each platform makes under `default`, with the action digest it keeps.
async fn needs_under(
    default: kbf_front::DefaultPlatform,
    cases: &[(&str, &[(&str, &str)])],
) -> Vec<kbf_caps::Request> {
    let script = Arc::new(Script::default());
    let farm = Farm::with_default_platform(Arc::clone(&script), default).await;
    for (argv, props) in cases {
        let job = job(argv, props, false);
        farm.upload(&job.blobs.iter().collect::<Vec<_>>()).await;
        start(&farm, &job.action).await.expect("Execute");
        let submitted = script.submitted();
        let key = &submitted.last().expect("submitted").request.key;
        let digest = kbf_types::ActionKey::try_from(key)
            .expect("an action's key")
            .action;
        assert_eq!(
            digest.hash_hex(),
            job.action.proto.hash,
            "{argv}: the default must not change the action digest"
        );
    }
    script
        .submitted()
        .into_iter()
        .map(|s| s.request.needs)
        .collect()
}

fn request(props: &[(&str, &str)]) -> kbf_caps::Request {
    kbf_caps::Request::parse(props.iter().copied()).unwrap()
}

/// Catches: an action that names no OS and no architecture left to match every node
/// (so a Mac that sorts first takes it); and the default changing what the client
/// sent: the action digest (so the cache key) must stay the client's. A property kbf
/// does not read (`pool`) and an empty platform both name nothing.
#[tokio::test]
async fn an_action_naming_no_os_or_arch_is_given_the_default() {
    let linux = [("os", "linux"), ("arch", "x86_64")];
    let cases: &[(&str, &[(&str, &str)])] = &[
        ("empty", &[]),
        ("pool", &[("pool", "mojo-bench")]),
        ("cpus only", &[("cpus", "2")]),
    ];
    let got = needs_under(kbf_front::DefaultPlatform::LINUX_X86_64, cases).await;
    let want = [request(&linux), request(&linux), {
        let mut both = linux.to_vec();
        both.push(("cpus", "2"));
        request(&both)
    }];
    assert_eq!(got, want);
}

/// Catches: the default applied where the client said something about where it runs:
/// `OSFamily=Darwin` alone or `ISA=arm-a64` alone (given the other half, a Linux
/// default would make a Mac action unservable or a conflict), an explicit Linux, an
/// ISA level alone, and what only a Mac reports (`xcode`, an iOS device), which a
/// Linux default would leave to wait out the unservable wait or refuse as a conflict.
#[tokio::test]
async fn a_request_that_says_where_it_runs_keeps_what_it_said() {
    let cases: &[(&str, &[(&str, &str)])] = &[
        ("darwin alone", &[("OSFamily", "Darwin")]),
        ("arm alone", &[("ISA", "arm-a64")]),
        ("explicit linux", &[("OSFamily", "Linux")]),
        ("isa level alone", &[("ISA", "x86-64-v3")]),
        ("xcode only", &[("xcode", "16C5032a")]),
        ("ios device", &[("ios.device", "1")]),
    ];
    let got = needs_under(kbf_front::DefaultPlatform::LINUX_X86_64, cases).await;
    let want = [
        request(&[("os", "macos")]),
        request(&[("arch", "arm64")]),
        request(&[("os", "linux")]),
        request(&[("arch", "x86_64"), ("isa_level", "x86-64-v3")]),
        request(&[("xcode", "16C5032a")]),
        request(&[("ios.device", "1")]),
    ];
    assert_eq!(got, want);
}

/// Catches: `none` still applying the built-in: the request stays empty, as it was
/// before the flag.
#[tokio::test]
async fn no_default_platform_leaves_the_request_as_the_client_made_it() {
    let cases: &[(&str, &[(&str, &str)])] = &[("empty", &[]), ("pool", &[("pool", "x")])];
    let got = needs_under(kbf_front::DefaultPlatform::NONE, cases).await;
    assert_eq!(got, [request(&[]), request(&[])]);
}
