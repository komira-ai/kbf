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
        // Near misses of the one name that passes unread: only `container-image`, exactly,
        // does. A prefix test (`starts_with`), a trim, or a case fold would let these in.
        (
            "container-image with a suffix letter",
            vec![("container-images", "docker://img@sha256:00")],
            "\"container-images\"",
        ),
        (
            "container-image with a trailing space",
            vec![("container-image ", "docker://img@sha256:00")],
            "\"container-image \"",
        ),
        (
            "container-image in capitals",
            vec![("CONTAINER-IMAGE", "docker://img@sha256:00")],
            "\"CONTAINER-IMAGE\"",
        ),
        (
            "container-image with a suffix",
            vec![("container-image-x", "docker://img@sha256:00")],
            "\"container-image-x\"",
        ),
        (
            "container-image with a leading space",
            vec![(" container-image", "docker://img@sha256:00")],
            "\" container-image\"",
        ),
        (
            "a prefix of container-image",
            vec![("container-imag", "docker://img@sha256:00")],
            "\"container-imag\"",
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

/// An Action whose platform is `props`, with no Command or input tree uploaded, as the
/// bare blob `Blob`s of an Action: the Command and the input root it names are absent.
fn action_with_nothing_else(argv: &str, props: &[(&str, &str)]) -> Blob {
    let command = Blob::of(&Command {
        arguments: vec![argv.to_owned()],
        ..Default::default()
    });
    let root = Blob::of(&Directory {
        files: vec![FileNode {
            name: "in".to_owned(),
            digest: Some(Blob::new("an input nobody uploaded").proto),
            ..Default::default()
        }],
        ..Default::default()
    });
    Blob::of(&Action {
        command_digest: Some(command.proto.clone()),
        input_root_digest: Some(root.proto.clone()),
        platform: Some(Platform {
            properties: props.iter().map(|(n, v)| property(n, v)).collect(),
        }),
        ..Default::default()
    })
}

/// Catches: a bad platform found only after the inputs are read, so a bad key beside
/// missing blobs is FAILED_PRECONDITION (MISSING, which a client answers by uploading and
/// retrying, and fails again) instead of INVALID_ARGUMENT. Only the Action is uploaded
/// here: the Command and the input root it names are absent, so a refusal after reading
/// them would be MISSING. A Command-level platform is read with the Command, so it is
/// refused before the input tree is walked (the root is absent here); when the Command
/// itself is absent nothing can be known, and the call is MISSING as it always was.
#[tokio::test]
async fn a_bad_platform_is_refused_before_the_inputs_are_read() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;

    let in_action = action_with_nothing_else("bad action platform", &[("pool", "darwin-sized")]);
    farm.upload(&[&in_action]).await;
    let status = start(&farm, &in_action).await.expect_err("refused");
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
    assert!(status.message().contains("\"pool\""), "{status:?}");

    // The same, a Command-level platform: the Command is uploaded, the input root is not.
    #[allow(deprecated)]
    let command = Blob::of(&Command {
        arguments: vec!["bad command platform".to_owned()],
        platform: Some(Platform {
            properties: vec![property("label.pool", "darwin")],
        }),
        ..Default::default()
    });
    // An input tree that is not there (the empty directory would be: the CAS holds the
    // empty blob), so reading the inputs before the platform would answer MISSING.
    let root = Blob::of(&Directory {
        files: vec![FileNode {
            name: "in".to_owned(),
            digest: Some(Blob::new("an input nobody uploaded").proto),
            ..Default::default()
        }],
        ..Default::default()
    });
    let in_command = Blob::of(&Action {
        command_digest: Some(command.proto.clone()),
        input_root_digest: Some(root.proto.clone()),
        ..Default::default()
    });
    farm.upload(&[&in_command, &command]).await;
    let status = start(&farm, &in_command).await.expect_err("refused");
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
    assert!(status.message().contains("\"label.pool\""), "{status:?}");

    // The Action's platform is read before the Command: a Command that does not decode
    // does not hide a bad key (the refusal names `pool`, not the Command).
    let garbage = Blob::new(vec![0xff, 0xff, 0xff]);
    let before_command = Blob::of(&Action {
        command_digest: Some(garbage.proto.clone()),
        input_root_digest: Some(Blob::of(&Directory::default()).proto),
        platform: Some(Platform {
            properties: vec![property("pool", "x")],
        }),
        ..Default::default()
    });
    farm.upload(&[&before_command, &garbage]).await;
    let status = start(&farm, &before_command).await.expect_err("refused");
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
    assert!(status.message().contains("\"pool\""), "{status:?}");

    // A good platform with the same blobs missing is still MISSING: the check does not
    // refuse what it should not.
    let fine = action_with_nothing_else("fine", &[("OSFamily", "linux")]);
    farm.upload(&[&fine]).await;
    let status = start(&farm, &fine).await.expect_err("inputs missing");
    assert_eq!(status.code(), Code::FailedPrecondition, "{status:?}");
    assert!(script.submitted().is_empty());
}

/// Catches: a cached result answered for an action whose platform kbf refuses (a mistyped
/// `pool=` would be quiet exactly when the action was run before), a Command-level
/// platform not looked at, and the opposite: a refusal for an action whose platform is
/// fine, one the CAS does not hold, or `GetActionResult` (a plain cache read) refusing.
#[tokio::test]
async fn a_cached_result_does_not_excuse_a_refused_platform() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let result = ActionResult::default();

    // Cached, platform refused: INVALID_ARGUMENT, not the result.
    let bad = job("cached bad", &[("pool", "darwin-sized")], false);
    farm.upload(&bad.blobs.iter().collect::<Vec<_>>()).await;
    farm.cache
        .write_action_result(Role::Daemon, bad.action.digest, &result)
        .await
        .expect("write");
    let status = start(&farm, &bad.action).await.expect_err("refused");
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
    assert!(status.message().contains("\"pool\""), "{status:?}");

    // The Command's platform is looked at when the Action carries none.
    #[allow(deprecated)]
    let command = Blob::of(&Command {
        arguments: vec!["cached bad command platform".to_owned()],
        platform: Some(Platform {
            properties: vec![property("Pool", "x")],
        }),
        ..Default::default()
    });
    let in_command = Blob::of(&Action {
        command_digest: Some(command.proto.clone()),
        input_root_digest: Some(Blob::of(&Directory::default()).proto),
        ..Default::default()
    });
    farm.upload(&[&in_command, &command]).await;
    farm.cache
        .write_action_result(Role::Daemon, in_command.digest, &result)
        .await
        .expect("write");
    let status = start(&farm, &in_command).await.expect_err("refused");
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
    assert!(status.message().contains("\"Pool\""), "{status:?}");

    // Cached, platform fine: answered from the cache, as before.
    let fine = job("cached fine", &[("OSFamily", "linux")], false);
    farm.upload(&fine.blobs.iter().collect::<Vec<_>>()).await;
    farm.cache
        .write_action_result(Role::Daemon, fine.action.digest, &result)
        .await
        .expect("write");
    let mut ops = start(&farm, &fine.action).await.expect("a hit");
    let only = next(&mut ops).await.expect("healthy").expect("an update");
    assert!(response(&only).cached_result);

    // Cached, the action not in the CAS: nothing to check, answered from the cache.
    let gone = job("cached, action not held", &[("pool", "x")], false);
    farm.cache
        .write_action_result(Role::Daemon, gone.action.digest, &result)
        .await
        .expect("write");
    let mut ops = start(&farm, &gone.action).await.expect("a hit");
    let only = next(&mut ops).await.expect("healthy").expect("an update");
    assert!(response(&only).cached_result);

    // GetActionResult is a plain cache read: it answers the refused action's result.
    let got = farm
        .ac()
        .get_action_result(kbf_proto::reapi::GetActionResultRequest {
            action_digest: Some(bad.action.proto.clone()),
            ..Default::default()
        })
        .await
        .expect("a plain cache read answers")
        .into_inner();
    assert_eq!(got, result);
    assert!(script.submitted().is_empty(), "a hit was submitted");
}

/// The default platform used by these tests, and the nodes they are placed against.
fn default_farm_defaults(
    platform: Option<kbf_caps::DefaultPlatform>,
) -> kbf_front::ExecutionDefaults {
    kbf_front::ExecutionDefaults {
        platform,
        ..kbf_front::ExecutionDefaults::default()
    }
}

fn node(os: &str, arch: &str) -> kbf_caps::NodeCaps {
    kbf_caps::NodeCaps::from_report([("os", os), ("arch", arch)]).expect("a node report")
}

/// Which of the four nodes of the farm the submitted request of job `n` can be placed on,
/// named `<os>/<arch>`.
fn placeable(script: &Script, n: usize) -> Vec<&'static str> {
    let needs = script.submitted()[n].request.needs.clone();
    [
        ("mac-a", "macos", "arm64"),
        ("mac-intel", "macos", "x86_64"),
        ("linux-arm", "linux", "arm64"),
        ("linux-x86", "linux", "x86_64"),
    ]
    .into_iter()
    .filter(|(_, os, arch)| needs.matches(&node(os, arch)))
    .map(|(name, ..)| name)
    .collect()
}

/// Catches: an action that names no platform placeable on a Mac (today the first node by
/// name, `mac-a`, takes it), the default applied to a request that names an OS, an
/// architecture, an ISA level, an Xcode or an iOS device (a half-named pair completed, a
/// Mac request turned into a Linux one), `none` not turning it off, the default changing
/// the action digest, and a platform kbf refuses defaulted instead of refused.
#[tokio::test]
async fn an_action_that_names_no_platform_is_placed_on_the_default() {
    let script = Arc::new(Script::default());
    let linux_x86 = Some(kbf_caps::DefaultPlatform::LINUX_X86_64);
    let farm = Farm::with_defaults(Arc::clone(&script), default_farm_defaults(linux_x86)).await;
    let mut jobs = Vec::new();
    for (why, props) in [
        ("empty", vec![]),
        ("only a booking", vec![("kbf-book-cpus", "2")]),
        (
            "container-image alone",
            vec![("container-image", "docker://img@sha256:00")],
        ),
        ("darwin alone", vec![("OSFamily", "Darwin")]),
        ("linux alone", vec![("OSFamily", "linux")]),
        ("arm64 alone", vec![("ISA", "arm64")]),
        ("amd64 alone", vec![("Arch", "amd64")]),
        ("isa level alone", vec![("isa_level", "x86-64-v3")]),
        ("xcode alone", vec![("xcode", "16C5032a")]),
        ("a device alone", vec![("ios.device", "1")]),
        (
            "a mac request",
            vec![("OSFamily", "Darwin"), ("ISA", "arm64")],
        ),
    ] {
        let job = job(why, &props, false);
        farm.upload(&job.blobs.iter().collect::<Vec<_>>()).await;
        start(&farm, &job.action).await.expect(why);
        jobs.push((why, job));
    }
    let submitted = script.submitted();
    for (n, (why, job)) in jobs.iter().enumerate() {
        assert_eq!(
            submitted[n].request.key.action, job.action.digest,
            "{why}: digest"
        );
    }
    let on = |n| placeable(&script, n);
    assert_eq!(
        on(0),
        ["linux-x86"],
        "empty: only a linux/x86_64 node, never mac-a"
    );
    assert_eq!(on(1), ["linux-x86"], "a booking alone names no platform");
    assert_eq!(
        on(2),
        ["linux-x86"],
        "container-image alone names no platform"
    );
    assert_eq!(
        on(3),
        ["mac-a", "mac-intel"],
        "OSFamily=Darwin alone is not completed"
    );
    assert_eq!(
        on(4),
        ["linux-arm", "linux-x86"],
        "OSFamily=linux alone is not completed"
    );
    assert_eq!(
        on(5),
        ["mac-a", "linux-arm"],
        "ISA=arm64 alone is not completed"
    );
    assert_eq!(
        on(6),
        ["mac-intel", "linux-x86"],
        "Arch=amd64 alone is not completed"
    );
    assert!(
        !on(7).contains(&"mac-a"),
        "an isa level alone is not completed: {:?}",
        on(7)
    );
    assert!(
        submitted[7].request.needs.exact("os").is_none(),
        "an isa level alone got an os"
    );
    assert!(
        submitted[8].request.needs.exact("os").is_none(),
        "xcode alone got an os"
    );
    assert!(submitted[9].request.needs.wants_device());
    assert!(
        submitted[9].request.needs.exact("os").is_none(),
        "a device alone got an os"
    );
    assert_eq!(on(10), ["mac-a"]);

    // A platform kbf refuses is refused, never defaulted.
    let bad = job("pool", &[("pool", "x")], false);
    farm.upload(&bad.blobs.iter().collect::<Vec<_>>()).await;
    let status = start(&farm, &bad.action).await.expect_err("refused");
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
    assert_eq!(script.submitted().len(), jobs.len());

    // `none`: today's behaviour, an empty platform matches every node.
    let script = Arc::new(Script::default());
    let farm = Farm::with_defaults(Arc::clone(&script), default_farm_defaults(None)).await;
    let empty = job("none: empty", &[], false);
    farm.upload(&empty.blobs.iter().collect::<Vec<_>>()).await;
    start(&farm, &empty.action).await.expect("Execute");
    assert_eq!(
        script.submitted()[0].request.needs,
        kbf_caps::Request::default()
    );
    assert_eq!(
        placeable(&script, 0),
        ["mac-a", "mac-intel", "linux-arm", "linux-x86"]
    );

    // Another default: macos/arm64.
    let script = Arc::new(Script::default());
    let mac = Some("macos/arm64".parse().expect("a platform"));
    let farm = Farm::with_defaults(Arc::clone(&script), default_farm_defaults(mac)).await;
    farm.upload(&empty.blobs.iter().collect::<Vec<_>>()).await;
    start(&farm, &empty.action).await.expect("Execute");
    assert_eq!(placeable(&script, 0), ["mac-a"]);
}

/// Catches: `container-image` on a Mac accepted, so the image is silently unused and the
/// action runs bare on the Mac; the refusal naming something other than the key and the OS;
/// a Linux or an unplaced `container-image` refused.
#[tokio::test]
async fn container_image_on_a_mac_action_is_refused() {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let image = ("container-image", "docker://img@sha256:00");
    for os in [("OSFamily", "Darwin"), ("os", "macos"), ("osfamily", "osx")] {
        let bad = job(os.1, &[image, os], false);
        farm.upload(&bad.blobs.iter().collect::<Vec<_>>()).await;
        let status = start(&farm, &bad.action).await.expect_err(os.1);
        assert_eq!(status.code(), Code::InvalidArgument, "{os:?}: {status:?}");
        let message = status.message();
        assert!(
            message.contains("\"container-image\"") && message.contains("\"macos\""),
            "{os:?}: {message}"
        );
    }
    assert!(script.submitted().is_empty());
    for (why, props) in [
        ("linux", vec![image, ("OSFamily", "Linux")]),
        ("no os", vec![image]),
    ] {
        let ok = job(why, &props, false);
        farm.upload(&ok.blobs.iter().collect::<Vec<_>>()).await;
        start(&farm, &ok.action).await.expect(why);
    }
}

/// The names in the backticks of the bullet list under `### Accepted property names` of
/// docs/platform-properties.md, `ios.device.<attribute>` expanded, and the text of the
/// paragraph that begins `Refused:`.
fn documented_names() -> (std::collections::BTreeSet<String>, String) {
    let docs = include_str!("../../../../docs/platform-properties.md");
    let section = docs
        .split("### Accepted property names")
        .nth(1)
        .expect("the section")
        .split("\n### ")
        .next()
        .expect("its end");
    let mut names = std::collections::BTreeSet::new();
    let mut refused = String::new();
    let mut in_refused = false;
    for line in section.lines() {
        if line.starts_with("Refused:") {
            in_refused = true;
        }
        if in_refused {
            refused.push_str(line);
            refused.push('\n');
            continue;
        }
        if !line.starts_with("- ") && !line.starts_with("  ") {
            continue;
        }
        for (i, token) in line.split('`').enumerate() {
            if i % 2 == 1 {
                if token == "ios.device.<attribute>" {
                    names.extend(
                        kbf_caps::DEVICE_ATTRIBUTES
                            .iter()
                            .map(|a| format!("ios.device.{a}")),
                    );
                } else {
                    names.insert(token.to_owned());
                }
            }
        }
    }
    (names, refused)
}

/// Catches: the list of accepted property names in docs/platform-properties.md drifting
/// from the code: a name added to (or removed from) what `kbf_caps::property_name` reads
/// and not the docs, a documented name the front refuses, `container-image` missing, or a
/// report-only key (`vm.slots`, ...) that the docs do not say is refused.
#[test]
fn the_documented_accepted_names_are_the_names_the_code_reads() {
    let (documented, refused) = documented_names();
    let mut code: std::collections::BTreeSet<String> =
        kbf_caps::property_names().into_iter().collect();
    for report_only in kbf_caps::REPORT_ONLY_KEYS {
        assert!(code.remove(report_only), "{report_only} not read");
        assert!(
            refused.contains(&format!("`{report_only}`")),
            "{report_only} is not in the Refused paragraph:\n{refused}"
        );
    }
    code.insert("container-image".to_owned());
    assert_eq!(
        documented, code,
        "docs/platform-properties.md (left) and the code (right) list different names"
    );
}
