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
/// (On macOS a GPU is the whole Mac's and is not booked on top; see
/// `a_gpu_on_macos_is_the_whole_macs`.)
#[tokio::test]
async fn property_names_are_read_in_any_case() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let props = [
        ("osfamily", "linux"),
        ("isa", "arm-a64"),
        ("GPU", "1"),
        ("KBF-Lease", "whole_machine"),
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
/// begin like a device key (`ios.devices`, `ios.simulator`, `iosx`) read as device keys
/// instead of refused as unknown.
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

    // Not device keys, so not read at all: refused naming them, not dropped.
    for name in ["ios.devices", "ios.simulator", "iosx"] {
        let other = job(name, &[(name, "1")], false);
        farm.upload(&other.blobs.iter().collect::<Vec<_>>()).await;
        let status = start(&farm, &other.action).await.expect_err(name);
        assert_eq!(status.code(), Code::InvalidArgument, "{name}: {status:?}");
        assert!(status.message().contains(name), "{name}: {status:?}");
    }
    assert_eq!(script.submitted().len(), 1);
}

/// Catches a GPU on macOS booked on top of a whole-machine lease (no Mac reports one,
/// so the GPU test would wait for ever), and a GPU on macOS taken outside a
/// whole-machine lease (it would run beside other work on a shared Mac; the front
/// refuses it, naming `kbf-lease=whole_machine`). An `os` property names macOS as
/// `OSFamily` does.
#[tokio::test]
async fn a_gpu_on_macos_is_the_whole_macs() {
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
        let shared = job(&format!("shared {os:?}"), &[os, ("gpu", "1")], false);
        farm.upload(&shared.blobs.iter().collect::<Vec<_>>()).await;
        let status = start(&farm, &shared.action)
            .await
            .expect_err("a shared GPU");
        assert_eq!(status.code(), Code::InvalidArgument);
        assert!(
            status.message().contains("kbf-lease=whole_machine"),
            "{status:?}"
        );
    }
    let submitted = script.submitted();
    assert_eq!(submitted.len(), 2);
    for s in &submitted {
        assert_eq!(s.request.kind, LeaseKind::WholeMachine);
        assert_eq!(
            s.request.resources,
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

/// Catches: a property kbf does not read dropped (so `pool=darwin-sized` placed the
/// action on any worker), a `label.<k>` key passing to be matched against node labels,
/// and a refusal that does not name the key. Each refusal must be INVALID_ARGUMENT
/// naming the property as sent, and nothing may reach the scheduler.
#[tokio::test]
async fn a_property_kbf_does_not_read_is_refused_naming_it() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    for (why, props, named) in [
        ("pool", vec![("pool", "darwin-sized")], "\"pool\""),
        ("Pool", vec![("Pool", "mojo-sized")], "\"Pool\""),
        (
            "pool beside a read key",
            vec![("OSFamily", "Darwin"), ("pool", "darwin-sized")],
            "\"pool\"",
        ),
        (
            "a label",
            vec![("label.pool", "darwin-sized")],
            "\"label.pool\"",
        ),
        (
            "a label in any case",
            vec![("LABEL.rack", "r1")],
            "\"LABEL.rack\"",
        ),
        (
            "container-image in another case",
            vec![("Container-Image", "docker://img@sha256:00")],
            "\"Container-Image\"",
        ),
        (
            "a made-up key",
            vec![("dockerNetwork", "off")],
            "\"dockerNetwork\"",
        ),
    ] {
        let bad = job(why, &props, false);
        farm.upload(&bad.blobs.iter().collect::<Vec<_>>()).await;
        let status = start(&farm, &bad.action).await.expect_err(why);
        assert_eq!(status.code(), Code::InvalidArgument, "{why}: {status:?}");
        assert!(status.message().contains(named), "{why}: {status:?}");
        assert!(status.details().is_empty(), "{why}: nothing to upload");
    }
    assert!(
        script.submitted().is_empty(),
        "a refused action reached the scheduler"
    );

    // Everything kbf reads, and `container-image` (read by the container driver), is
    // accepted: this is the accepted list in docs/platform-properties.md.
    let accepted = [
        ("container-image", "docker://img@sha256:00"),
        ("OSFamily", "linux"),
        ("ISA", "x86-64"),
        ("kbf-book-cpus", "2"),
        ("kbf-book-mem-gib", "2"),
        ("cpu.model", "epyc"),
    ];
    let ok = job("accepted", &accepted, false);
    farm.upload(&ok.blobs.iter().collect::<Vec<_>>()).await;
    start(&farm, &ok.action).await.expect("accepted keys");
    for (key, value) in [("gpu", "0"), ("kbf-lease", "action"), ("Arch", "amd64")] {
        let ok = job(key, &[(key, value)], false);
        farm.upload(&ok.blobs.iter().collect::<Vec<_>>()).await;
        start(&farm, &ok.action).await.expect(key);
    }
}

/// Catches: a Mac action (`OSFamily=Darwin`, `ISA=arm64`) whose OS or architecture is
/// lost between the request and the scheduler, so a Linux node could take it. The
/// needs the front hands the scheduler must be satisfied by a Mac node and by no Linux
/// node (either architecture) and no Intel Mac.
#[tokio::test]
async fn a_mac_action_is_never_placeable_on_a_linux_node() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let mac = job("mac", &[("OSFamily", "Darwin"), ("ISA", "arm64")], false);
    farm.upload(&mac.blobs.iter().collect::<Vec<_>>()).await;
    start(&farm, &mac.action).await.expect("Execute");
    let needs = script.submitted()[0].request.needs.clone();

    let node = |os, arch| kbf_caps::NodeCaps::from_report([("os", os), ("arch", arch)]).unwrap();
    assert!(
        needs.matches(&node("macos", "arm64")),
        "the Mac it asked for"
    );
    for (os, arch) in [("linux", "arm64"), ("linux", "x86_64"), ("macos", "x86_64")] {
        assert!(!needs.matches(&node(os, arch)), "placeable on {os}/{arch}");
    }
}
