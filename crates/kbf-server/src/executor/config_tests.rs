use std::time::Duration;

use js_proto::v1::ExecMode;

use super::{Config, ConfigError, InputTree, KEY_LIMIT};
use crate::executor::PutDirRoots;

fn base() -> Config {
    Config::new("front-1")
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

/// Catches each setting the executor's safety or the job service rejects being accepted:
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
                prepare_limit: Duration::ZERO,
                ..base()
            },
            ConfigError::Bounds,
        ),
        (
            Config {
                poll: Duration::ZERO,
                ..base()
            },
            ConfigError::Bounds,
        ),
        (
            Config {
                live: Duration::ZERO,
                ..base()
            },
            ConfigError::Bounds,
        ),
    ];
    for (config, want) in rows {
        assert_eq!(config.validate(), Err(want.clone()), "{want}");
    }
}

/// Catches a wall limit that depends on anything but the action's timeout: it is the timeout
/// and the run margin.
#[test]
fn a_jobs_wall_limit_is_the_actions_timeout_and_the_margin() {
    let config = base();
    assert_eq!(
        config.wall(Duration::from_secs(60)),
        Duration::from_secs(60) + config.run_margin
    );
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

/// Catches an instance id that is shared by two processes or empty: two configs made one after
/// the other carry different ids, each a non-empty label value of hex.
#[test]
fn each_config_gets_an_instance_id_of_its_own() {
    let (one, two) = (base(), base());
    assert!(!one.instance.is_empty());
    assert_ne!(one.instance, two.instance);
    assert!(one.instance.chars().all(|c| c.is_ascii_hexdigit()));
}

/// Catches defaults that drift: the liveness row outlives its renewal by 20 s, a finished
/// operation is answered from memory for ten minutes, and the poll is a quarter second.
#[test]
fn the_defaults_name_the_live_time_the_retention_and_the_poll() {
    let config = base();
    assert_eq!(config.live, Duration::from_secs(20));
    assert_eq!(config.finished_retention, Duration::from_secs(600));
    assert_eq!(config.poll, Duration::from_millis(250));
    assert_eq!(config.frontend, "front-1");
}

/// Catches an operator message that names the wrong setting: the text of the bounds refusal and
/// of the key-ceiling refusals, whole.
#[test]
fn the_messages_of_the_refusals_name_the_setting_and_the_numbers() {
    assert_eq!(
        ConfigError::Bounds.to_string(),
        "the poll interval, the call timeout, the preparation limit and the liveness time must be above zero"
    );
    assert_eq!(
        ConfigError::KeyCeiling.to_string(),
        "the longest job's wall limit exceeds the key ceiling"
    );
    assert_eq!(
        ConfigError::KeyLimit.to_string(),
        "the key ceiling and the longest job's wall limit must be at most 4 hours"
    );
    assert_eq!(KEY_LIMIT, Duration::from_secs(4 * 3600));
}

/// Asserts a config with a pool of Macs is checked by the pool's rules too, after its own:
/// a valid pool passes, an invalid one is refused naming it, and a config broken elsewhere is
/// refused for that first.
#[test]
fn a_pool_of_macs_is_checked_with_the_config() {
    let mac = crate::executor::MacPool {
        bundle: BUNDLE.to_owned(),
        os_version: 26,
        roots: None,
    };
    let with = |mac| Config {
        mac: Some(mac),
        ..base()
    };
    assert_eq!(with(mac.clone()).validate(), Ok(()));
    let bad = crate::executor::MacPool {
        os_version: 0,
        ..mac.clone()
    };
    assert_eq!(
        with(bad.clone()).validate(),
        Err(ConfigError::Mac(crate::executor::MacPoolError::OsVersion))
    );
    let broken = Config {
        frontend: String::new(),
        ..with(bad)
    };
    assert_eq!(broken.validate(), Err(ConfigError::Frontend));
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
