use std::time::Duration;

use kbf_sched::fence::{LEADER_LEASE_MARGIN, LEASE_GRACE, START_GRACE, START_VALIDITY};

use js_proto::v1::ExecMode;

use super::{Config, ConfigError, Delivery, HARD_CAP_PAD, PRESIGN_LIMIT};
use crate::config::MAX_HEARTBEAT_INTERVAL_MS;

const GIB: u64 = 1 << 30;

fn base() -> Config {
    Config {
        insecure_argv_secrets: true,
        ..Config::new("front-1", 2, 16_000, 64 * GIB)
    }
}

/// Catches a default set that the executor itself refuses (a start that fails at once), and
/// secrets in arguments that nobody asked for: a config built from the defaults is refused
/// until the operator says the secrets may travel as arguments, and is valid once they do.
#[test]
fn the_defaults_are_valid_once_the_operator_allows_argv_secrets() {
    let defaults = Config::new("front-1", 2, 16_000, 64 * GIB);
    assert!(!defaults.insecure_argv_secrets);
    assert_eq!(defaults.validate(), Err(ConfigError::InsecureArgv));
    assert_eq!(base().validate(), Ok(()));
}

/// Catches a run booking the wrong scratch: the inputs, the output allowance and the fixed
/// margin, each counted once, and the allowance and the margin alone when the size of the inputs
/// is not known. The numbers are not round (so a sum that drops a term cannot match), and the
/// defaults are the documented 8 GiB and 1 GiB.
#[test]
fn a_runs_disk_is_its_inputs_plus_the_allowance_plus_the_margin() {
    use super::{DEFAULT_OUTPUT_DISK, DISK_MARGIN};
    assert_eq!(DEFAULT_OUTPUT_DISK, 8 * GIB);
    assert_eq!(DISK_MARGIN, GIB);
    let config = Config {
        output_disk_bytes: 5 * GIB + 7,
        ..base()
    };
    assert_eq!(
        config.disk_bytes(Some(300)),
        300 + 5 * GIB + 7 + DISK_MARGIN
    );
    assert_eq!(config.disk_bytes(Some(0)), 5 * GIB + 7 + DISK_MARGIN);
    assert_eq!(config.disk_bytes(None), 5 * GIB + 7 + DISK_MARGIN);
    assert_eq!(base().disk_bytes(None), DEFAULT_OUTPUT_DISK + DISK_MARGIN);
    let huge = Config {
        output_disk_bytes: u64::MAX,
        ..base()
    };
    assert_eq!(huge.disk_bytes(Some(u64::MAX)), u64::MAX, "no wrap");
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
                presign_ceiling: Duration::from_secs(3299),
                ..base()
            },
            ConfigError::Presign,
        ),
        (
            Config {
                presign_ceiling: PRESIGN_LIMIT + Duration::from_secs(1),
                ..base()
            },
            ConfigError::PresignLimit,
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
                insecure_argv_secrets: false,
                ..base()
            },
            ConfigError::InsecureArgv,
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
    let hello = base().hello("js:node1", "inst", "pred");
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

/// Catches the insecure flag demanded of a delivery that sends no secret in arguments, and
/// dropped from the one that does: `argv` is refused without it, `files` is valid without it.
#[test]
fn files_mode_needs_no_insecure_flag_and_argv_mode_still_does() {
    let files = Config {
        delivery: Delivery::Files,
        insecure_argv_secrets: false,
        ..base()
    };
    assert_eq!(files.validate(), Ok(()));
    let argv = Config {
        delivery: Delivery::InsecureArgv,
        insecure_argv_secrets: false,
        ..base()
    };
    assert_eq!(argv.validate(), Err(ConfigError::InsecureArgv));
}

/// Catches the parser being the only guard against the contradiction: a config that says the
/// secrets travel as files and also that they may travel as arguments is refused by name.
#[test]
fn files_delivery_refuses_the_insecure_opt_in() {
    let both = Config {
        delivery: Delivery::Files,
        insecure_argv_secrets: true,
        ..base()
    };
    assert_eq!(both.validate(), Err(ConfigError::InsecureWithFiles));
}

/// Catches a job that asks its host for the wrong system, architecture or way to run: a job
/// asks for linux, the architecture the config names (not a fixed one) and a container, and
/// for nothing else, whatever the delivery of the secrets.
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
        delivery: Delivery::Files,
        insecure_argv_secrets: false,
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
