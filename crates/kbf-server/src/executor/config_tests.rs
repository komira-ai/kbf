use std::time::Duration;

use kbf_sched::fence::{LEADER_LEASE_MARGIN, LEASE_GRACE, START_GRACE, START_VALIDITY};

use js_proto::v1::ExecMode;

use super::{Config, ConfigError, HARD_CAP_PAD, InputTree, KEY_LIMIT, Ownership};
use crate::config::MAX_HEARTBEAT_INTERVAL_MS;
use crate::executor::PutDirRoots;

const GIB: u64 = 1 << 30;

fn base() -> Config {
    Config::new("front-1", 2, 16_000, 64 * GIB)
}

/// Catches a default set that the executor itself refuses (a start that fails at once), and a
/// default that gives runs somewhere to commit that nobody named: a config built from the
/// defaults is valid, has no put-directory roots, and has the longest key ceiling.
#[test]
fn the_defaults_are_valid_and_name_no_put_directory_roots() {
    let defaults = base();
    assert_eq!(defaults.validate(), Ok(()));
    assert_eq!(defaults.task_out, None);
    assert_eq!(defaults.key_ceiling, KEY_LIMIT);
}

/// Catches the heartbeat interval returning to 5 s: a `Start` names the newest heartbeat the
/// farm took and is valid for 14 s from it, so each second of that heartbeat's age is a second
/// less for the preparation and the submit. The default is 1 s, and the config holding it is
/// valid.
#[test]
fn the_default_heartbeat_interval_is_one_second() {
    let config = base();
    assert_eq!(config.heartbeat, Duration::from_secs(1));
    assert_eq!(config.validate(), Ok(()));
}

/// Catches a fence time that lets a silent worker's runs outlive the scheduler's grace
/// (the farm requeues the action after G while the first copy still runs): T plus the
/// scheduler's margin plus one step must stay under G, for the default T and for no bigger.
#[test]
fn the_fence_time_stays_under_the_schedulers_grace() {
    let config = base();
    assert!(
        config.fence_after + LEADER_LEASE_MARGIN + config.poll < LEASE_GRACE,
        "the default T is not under G"
    );
    let at_the_edge = |fence_after| Config {
        fence_after,
        ..base()
    };
    // The step is long here, so the grace is the rule that binds, and not the start window.
    let slow = |fence_after| Config {
        fence_after,
        poll: Duration::from_secs(20),
        ..base()
    };
    let room = LEASE_GRACE - LEADER_LEASE_MARGIN - Duration::from_secs(20);
    assert_eq!(slow(room - Duration::from_millis(1)).validate(), Ok(()));
    assert_eq!(slow(room).validate(), Err(ConfigError::Fence));
    assert_eq!(at_the_edge(LEASE_GRACE).validate(), Err(ConfigError::Fence));
}

/// Catches a fence time under which a `Start` posted at the very end of its window is stopped
/// after the farm has run the action again: the window, T and the scheduler's margin must
/// stay under the start grace. T = 50 s passes the grace rule (55.5 s under 60 s) and breaks
/// this one (69 s over 60 s): the boundary is at 41 s.
#[test]
fn the_fence_time_leaves_a_start_posted_at_the_end_of_its_window_its_stop() {
    let with = |fence_after| Config {
        fence_after,
        ..base()
    };
    let room = START_GRACE - START_VALIDITY - LEADER_LEASE_MARGIN;
    assert_eq!(room, Duration::from_secs(41));
    assert_eq!(with(room - Duration::from_millis(1)).validate(), Ok(()));
    assert_eq!(with(room).validate(), Err(ConfigError::StartWindow));
    assert_eq!(
        with(Duration::from_secs(50)).validate(),
        Err(ConfigError::StartWindow)
    );
}

/// Catches each setting the executor's safety or the farm's wire rejects being accepted:
/// one defect per row, and the error that names it.
#[test]
fn each_unsafe_setting_is_refused_by_name() {
    let rows: Vec<(Config, ConfigError)> = vec![
        (
            Config {
                frontend: String::new(),
                ..base()
            },
            ConfigError::Frontend,
        ),
        (
            Config {
                frontend: "a b".to_owned(),
                ..base()
            },
            ConfigError::Frontend,
        ),
        (
            Config {
                frontend: "x".repeat(64),
                ..base()
            },
            ConfigError::Frontend,
        ),
        (Config { hosts: 0, ..base() }, ConfigError::NoHosts),
        (
            Config {
                cpu_millis: 999,
                ..base()
            },
            ConfigError::Shape,
        ),
        (Config { slots: 0, ..base() }, ConfigError::Shape),
        (
            Config {
                slots: 257,
                ..base()
            },
            ConfigError::Shape,
        ),
        (
            Config {
                memory_bytes: 4 * GIB,
                slots: 8,
                ..base()
            },
            ConfigError::Memory(8),
        ),
        (
            Config {
                heartbeat: Duration::ZERO,
                ..base()
            },
            ConfigError::Heartbeat,
        ),
        (
            Config {
                heartbeat: Duration::from_millis(MAX_HEARTBEAT_INTERVAL_MS + 1),
                ..base()
            },
            ConfigError::Heartbeat,
        ),
        (
            Config {
                heartbeat: Duration::from_secs(40),
                fence_after: Duration::from_secs(40),
                ..base()
            },
            ConfigError::Heartbeat,
        ),
        (
            Config {
                heartbeat: Duration::from_secs(7),
                fence_after: Duration::from_secs(7),
                ..base()
            },
            ConfigError::FenceShort,
        ),
        (
            Config {
                default_timeout: Duration::ZERO,
                ..base()
            },
            ConfigError::Timeout,
        ),
        (
            Config {
                default_timeout: Duration::from_secs(3001),
                ..base()
            },
            ConfigError::Timeout,
        ),
        (
            Config {
                key_ceiling: Duration::from_secs(3299),
                ..base()
            },
            ConfigError::KeyCeiling,
        ),
        (
            Config {
                key_ceiling: KEY_LIMIT + Duration::from_secs(1),
                ..base()
            },
            ConfigError::KeyLimit,
        ),
        (
            Config {
                call_timeout: Duration::ZERO,
                ..base()
            },
            ConfigError::Bounds,
        ),
        (
            Config {
                call_timeout: Duration::from_secs(40),
                ..base()
            },
            ConfigError::Bounds,
        ),
        (
            Config {
                pass_budget: Duration::ZERO,
                ..base()
            },
            ConfigError::Bounds,
        ),
        (
            Config {
                prepare_limit: Duration::ZERO,
                ..base()
            },
            ConfigError::Bounds,
        ),
        (
            Config {
                prepare_concurrency: 0,
                ..base()
            },
            ConfigError::Bounds,
        ),
    ];
    for (config, want) in rows {
        assert_eq!(config.validate(), Err(want.clone()), "{want}");
    }
}

/// Catches a Hello that books more memory than a host of the shape holds: the runs the
/// farm may book on the host, capped at 1.5 times the booking plus the padding each, must
/// fit the shape; and a host that books nothing is refused.
#[test]
fn the_booked_memory_leaves_room_for_the_hard_caps() {
    let config = base();
    let booked = config.booked_memory_gib() * GIB;
    assert_eq!(config.booked_memory_gib(), 40);
    assert!(booked * 3 / 2 + config.slots * HARD_CAP_PAD <= config.memory_bytes);
    // One GiB more would not fit: the arithmetic is tight, not merely safe.
    assert!((booked + GIB) * 3 / 2 + config.slots * HARD_CAP_PAD > config.memory_bytes);
    let tiny = Config {
        memory_bytes: GIB,
        ..base()
    };
    assert_eq!(tiny.booked_memory_gib(), 0);
}

/// Catches a hard cap that is not the container driver's formula (an out-of-memory run would
/// then be a different event on the two executors), and a wall time that depends on anything
/// but the action's timeout.
#[test]
fn a_run_is_capped_as_the_container_driver_caps_it() {
    assert_eq!(Config::hard_memory(2 * GIB), 3 * GIB + HARD_CAP_PAD);
    assert_eq!(Config::hard_memory(u64::MAX), u64::MAX / 2 + HARD_CAP_PAD);
    let config = base();
    assert_eq!(
        config.wall(Duration::from_secs(60)),
        Duration::from_secs(60) + config.run_margin
    );
}

/// Catches a Hello that places a VM lease on a host or no whole-machine lease (a driver other
/// than `container` and `native-whole-machine`, or one of them missing), that omits the slot
/// cap, or that carries the wrong capacity.
#[test]
fn the_hello_reports_a_container_and_whole_machine_host_and_nothing_else() {
    let hello = base().hello(
        crate::executor::mac::WorkerOs::Linux,
        "js:node1",
        "inst",
        "pred",
    );
    let entries: Vec<(&str, &str)> = hello
        .capabilities
        .iter()
        .map(|c| (c.key.as_str(), c.value.as_str()))
        .collect();
    assert_eq!(
        entries,
        [
            ("action_slots", "8"),
            ("arch", "x86_64"),
            ("cpus", "16"),
            ("drivers", "container"),
            ("drivers", "native-whole-machine"),
            ("mem_gib", "40"),
            ("os", "linux"),
        ]
    );
    assert_eq!(hello.node_id, "js:node1");
    assert_eq!(hello.instance_id, "inst");
    assert_eq!(hello.predecessor_instance_id, "pred");
    let reported = crate::worker::reported(&hello, crate::worker::HostFloor::DEFAULT)
        .expect("the farm reads the hello");
    assert_eq!(reported.capacity.cpu_millis, 16_000);
    assert_eq!(reported.capacity.memory_bytes, 40 * GIB);
}

/// Catches a job that asks its host for the wrong system, architecture or way to run: a job
/// asks for linux, the architecture the config names (not a fixed one) and a container, and
/// for nothing else.
#[test]
fn a_job_asks_for_a_linux_host_of_the_configured_arch_that_runs_containers() {
    let wanted = |config: &Config| {
        let constraints = config.constraints();
        (
            constraints.os,
            constraints.arch,
            constraints.exec == i32::from(ExecMode::Container),
        )
    };
    assert_eq!(
        wanted(&base()),
        ("linux".to_owned(), "x86_64".to_owned(), true)
    );
    let arm = Config {
        arch: "aarch64".to_owned(),
        ..base()
    };
    assert_eq!(
        wanted(&arm),
        ("linux".to_owned(), "aarch64".to_owned(), true)
    );
}

/// Catches a watch over another process's jobs that is shorter than a live executor's longest
/// pass: it would cancel the jobs of a process that is alive and has not yet had the chance to
/// submit. The watch is the fence time (the longest a pass is held) and one step.
#[test]
fn the_watch_over_earlier_jobs_is_the_fence_time_and_a_step() {
    let config = base();
    assert_eq!(config.adopt_watch(), config.fence_after + config.poll);
    let slower = Config {
        fence_after: Duration::from_secs(20),
        poll: Duration::from_secs(3),
        ..base()
    };
    assert_eq!(slower.adopt_watch(), Duration::from_secs(23));
}

/// Catches an instance id that is shared by two processes or empty: two configs made one after
/// the other carry different ids, each a non-empty label value of hex.
#[test]
fn each_config_gets_an_instance_id_of_its_own() {
    let (one, two) = (base(), base());
    assert!(!one.instance.is_empty());
    assert_ne!(one.instance, two.instance);
    assert!(one.instance.chars().all(|c| c.is_ascii_hexdigit()));
}

/// A config of shared ownership that is valid: put directories under roots, and the default
/// liveness time.
fn shared() -> Config {
    Config {
        ownership: Ownership::Shared,
        task_out: Some(PutDirRoots::new("/mnt/ts/put").expect("roots")),
        ..base()
    }
}

/// Catches a default that turns on shared ownership, or a liveness time that is not the 45 s
/// the flag documents (the default fence time and the scheduler's margin): a config from
/// `Config::new` is single, is not `shared()`, and keeps its liveness row for fence + margin.
#[test]
fn the_defaults_are_single_ownership_and_a_live_time_of_the_fence_plus_the_margin() {
    let config = base();
    assert_eq!(config.ownership, Ownership::Single);
    assert_eq!(Ownership::default(), Ownership::Single);
    assert!(!config.shared());
    assert_eq!(config.live, Duration::from_secs(45));
    assert_eq!(config.live, config.fence_after + LEADER_LEASE_MARGIN);
    assert!(shared().shared());
    assert_eq!(shared().validate(), Ok(()));
}

/// Catches a liveness time under which a row may expire while its process still acts, and an
/// off-by-one at the edge: the time must be at least the fence time plus the scheduler's
/// margin, so for the default fence exactly 45 s is valid and a second less is refused, and
/// the edge moves with the fence time.
#[test]
fn live_secs_below_fence_plus_margin_is_refused() {
    let with = |fence_after: Duration, live: Duration| Config {
        fence_after,
        live,
        ..shared()
    };
    let fence = base().fence_after;
    assert_eq!(fence, Duration::from_secs(40));
    let edge = fence + LEADER_LEASE_MARGIN;
    assert_eq!(with(fence, edge).validate(), Ok(()));
    assert_eq!(
        with(fence, edge - Duration::from_secs(1)).validate(),
        Err(ConfigError::Live)
    );
    assert_eq!(
        with(fence, Duration::ZERO).validate(),
        Err(ConfigError::Live)
    );
    assert_eq!(with(fence, edge * 4).validate(), Ok(()));
    let short = Duration::from_secs(30);
    assert_eq!(with(short, short + LEADER_LEASE_MARGIN).validate(), Ok(()));
    assert_eq!(
        with(short, short + LEADER_LEASE_MARGIN - Duration::from_secs(1)).validate(),
        Err(ConfigError::Live)
    );
}

/// Catches a liveness time judged where nothing uses it: with single ownership no liveness
/// row is written, so a time under the fence time, or zero, is valid.
#[test]
fn single_ownership_does_not_judge_the_live_time() {
    for live in [Duration::ZERO, Duration::from_secs(1)] {
        let config = Config { live, ..base() };
        assert_eq!(config.validate(), Ok(()), "{live:?}");
    }
}

/// Catches a worker name that two live processes of one frontend id could both register: a
/// single frontend names its workers `js:<frontend>-<n>`, a shared one adds the first 8
/// characters of its instance, so two instances give different names, and an instance
/// shorter than 8 characters is used whole.
#[test]
fn worker_names_carry_the_instance_only_in_shared_mode() {
    let single = Config {
        instance: "0123456789abcdef".to_owned(),
        ..base()
    };
    assert_eq!(single.worker_name(0), "js:front-1-0");
    assert_eq!(single.worker_name(7), "js:front-1-7");
    let one = Config {
        instance: "0123456789abcdef".to_owned(),
        ..shared()
    };
    assert_eq!(one.worker_name(0), "js:front-1.01234567-0");
    assert_eq!(one.worker_name(3), "js:front-1.01234567-3");
    let two = Config {
        instance: "fedcba9876543210".to_owned(),
        ..shared()
    };
    assert_eq!(two.worker_name(0), "js:front-1.fedcba98-0");
    assert_ne!(one.worker_name(0), two.worker_name(0));
    let short = Config {
        instance: "abc".to_owned(),
        ..shared()
    };
    assert_eq!(short.worker_name(1), "js:front-1.abc-1");
    let exact = Config {
        instance: "01234567".to_owned(),
        ..shared()
    };
    assert_eq!(exact.worker_name(1), "js:front-1.01234567-1");
}

/// Catches a Mac worker name that two live processes of one group could both register, or
/// that one frontend id could make a Linux name: the Macs' workers are `js-mac:` where the
/// Linux ones are `js:`, and carry the first 8 characters of the instance only with shared
/// ownership, as the Linux ones do.
#[test]
fn mac_worker_names_carry_the_instance_only_in_shared_mode() {
    use crate::executor::mac::WorkerOs;
    let single = Config {
        instance: "0123456789abcdef".to_owned(),
        ..base()
    };
    assert_eq!(single.worker_of(WorkerOs::Mac, 0), "js-mac:front-1-0");
    assert_eq!(single.worker_of(WorkerOs::Linux, 2), single.worker_name(2));
    let one = Config {
        instance: "0123456789abcdef".to_owned(),
        ..shared()
    };
    assert_eq!(one.worker_of(WorkerOs::Mac, 1), "js-mac:front-1.01234567-1");
    assert_eq!(one.worker_of(WorkerOs::Linux, 1), "js:front-1.01234567-1");
    let two = Config {
        instance: "fedcba9876543210".to_owned(),
        ..shared()
    };
    assert_ne!(
        one.worker_of(WorkerOs::Mac, 0),
        two.worker_of(WorkerOs::Mac, 0)
    );
}

/// Catches an operator message that names the wrong flag or drops the number: the text of
/// the liveness, state-store and key-ceiling refusals, whole.
#[test]
fn the_messages_of_the_refusals_name_the_flags_and_the_numbers() {
    assert_eq!(
        ConfigError::KeyCeiling.to_string(),
        "the longest job's wall limit exceeds the key ceiling"
    );
    assert_eq!(
        ConfigError::KeyLimit.to_string(),
        "the key ceiling and the longest job's wall limit must be at most 4 hours"
    );
    assert_eq!(KEY_LIMIT, Duration::from_secs(4 * 3600));
    assert_eq!(
        ConfigError::Live.to_string(),
        "--executor-live-secs must be at least the fence time plus 5 s with \
         --executor-ownership shared"
    );
    assert_eq!(LEADER_LEASE_MARGIN.as_secs(), 5);
    assert_eq!(
        ConfigError::SharedStore.to_string(),
        "--executor-ownership shared needs --state-store: the rows of the jobs are kept there"
    );
}
/// Asserts the Hello of a Mac worker: `os=macos`, `arch=arm64` whatever the Linux arch, and
/// the one driver `native`, so the farm places a Mac action on it and no `whole_machine` lease;
/// and that the farm reads it and finds a Mac action's request met and a Linux one's not.
/// Catches a Mac worker that reports Linux, or that offers a whole machine.
#[test]
fn a_mac_workers_hello_reports_a_native_arm64_mac() {
    let cfg = Config {
        arch: "x86_64".to_owned(),
        ..base()
    };
    let hello = cfg.hello(
        crate::executor::mac::WorkerOs::Mac,
        "js-mac:node1-0",
        "inst",
        "",
    );
    let entries: Vec<(&str, &str)> = hello
        .capabilities
        .iter()
        .map(|c| (c.key.as_str(), c.value.as_str()))
        .collect();
    assert_eq!(
        entries,
        [
            ("action_slots", "8"),
            ("arch", "arm64"),
            ("cpus", "16"),
            ("drivers", "native"),
            ("mem_gib", "40"),
            ("os", "macos"),
        ]
    );
    let reported = crate::worker::reported(&hello, crate::worker::HostFloor::DEFAULT)
        .expect("the farm reads the hello");
    let mac = kbf_caps::Request::from_platform([("OSFamily", "macos")]).expect("mac");
    let linux = kbf_caps::Request::from_platform([("OSFamily", "linux")]).expect("linux");
    assert!(mac.matches(&reported.caps), "a Mac action is placed on it");
    assert!(!linux.matches(&reported.caps), "a Linux action is not");
    assert_eq!(reported.caps.drivers, ["native".to_owned()].into());
}

/// Asserts a config with a pool of Macs is checked by the pool's rules too, after its own:
/// a valid pool passes, an invalid one is refused naming it, and a config broken elsewhere is
/// refused for that first.
#[test]
fn a_pool_of_macs_is_checked_with_the_config() {
    let mac = crate::executor::MacPool {
        hosts: 1,
        bundle: BUNDLE.to_owned(),
        os_version: 26,
        roots: None,
    };
    let with = |mac| Config {
        mac: Some(mac),
        ..base()
    };
    assert_eq!(with(mac.clone()).validate(), Ok(()));
    let none = crate::executor::MacPool {
        hosts: 0,
        ..mac.clone()
    };
    assert_eq!(
        with(none).validate(),
        Err(ConfigError::Mac(crate::executor::MacPoolError::NoHosts))
    );
    let broken = Config {
        hosts: 0,
        ..with(mac)
    };
    assert_eq!(broken.validate(), Err(ConfigError::NoHosts));
    assert_eq!(base().mac, None);
}

/// The macOS `kbf-task` bundle of a release, by its manifest digest.
const BUNDLE: &str =
    "kbf-task@sha256:9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a";

/// Asserts the default of `--executor-input-tree`: `Config::new` and `InputTree::default()` are
/// `Manifests`, and the default config is valid with or without roots. Catches: a default
/// that turns the tree list on for every frontend that never asked for it.
#[test]
fn the_default_input_tree_is_manifests() {
    assert_eq!(base().input_tree, InputTree::Manifests);
    assert_eq!(InputTree::default(), InputTree::Manifests);
    let put_dir = Config {
        task_out: Some(PutDirRoots::new("/mnt/ts/put").expect("roots")),
        ..base()
    };
    assert_eq!(put_dir.input_tree, InputTree::Manifests);
    assert_eq!(put_dir.validate(), Ok(()));
}
