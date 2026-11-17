use std::time::Duration;

use js_proto::v1::ExecMode;

use js_proto::v1::NetworkLevel;

use super::{Config, ConfigError, Delivery, InputTree, Network, PRESIGN_LIMIT, TaskOut};
use crate::executor::PutDirRoots;

fn base() -> Config {
    Config {
        insecure_argv_secrets: true,
        ..Config::new("front-1")
    }
}

/// Catches a default set that the executor itself refuses (a start that fails at once), and
/// secrets in arguments that nobody asked for: a config built from the defaults is refused
/// until the operator says the secrets may travel as arguments, and is valid once they do.
#[test]
fn the_defaults_are_valid_once_the_operator_allows_argv_secrets() {
    let defaults = Config::new("front-1");
    assert!(!defaults.insecure_argv_secrets);
    assert_eq!(defaults.validate(), Err(ConfigError::InsecureArgv));
    assert_eq!(base().validate(), Ok(()));
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

/// Catches an operator message that names the wrong setting: the text of the bounds refusal.
#[test]
fn the_bounds_refusal_names_the_liveness_time() {
    assert_eq!(
        ConfigError::Bounds.to_string(),
        "the poll interval, the call timeout, the preparation limit and the liveness time must be above zero"
    );
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
/// `Manifests`, and the default config is valid with either task-out mode. Catches: a default
/// that turns the tree list on for every frontend that never asked for it.
#[test]
fn the_default_input_tree_is_manifests() {
    assert_eq!(base().input_tree, InputTree::Manifests);
    assert_eq!(InputTree::default(), InputTree::Manifests);
    let put_dir = Config {
        task_out: TaskOut::PutDir(PutDirRoots::new("/mnt/ts/put").expect("roots")),
        ..base()
    };
    assert_eq!(put_dir.input_tree, InputTree::Manifests);
    assert_eq!(put_dir.validate(), Ok(()));
}

/// Asserts that the provider with presigned task outputs is refused as
/// `ConfigError::ProviderPresigned` with its exact message naming both flags, that the same
/// config with a put directory validates, and that presigned outputs with the manifests still
/// validate. Catches: a provider tree accepted where no job reads its root from a tree, and a
/// refusal that does not say which flag to change.
#[test]
fn the_provider_input_tree_needs_put_dir_outputs() {
    let presigned = Config {
        task_out: TaskOut::Presigned,
        input_tree: InputTree::Provider,
        ..base()
    };
    assert_eq!(presigned.validate(), Err(ConfigError::ProviderPresigned));
    assert_eq!(
        ConfigError::ProviderPresigned.to_string(),
        "--executor-input-tree provider needs --task-out-mode put-dir: only a put-directory \
         job reads its root from a tree"
    );
    let put_dir = Config {
        task_out: TaskOut::PutDir(PutDirRoots::new("/mnt/ts/put").expect("roots")),
        input_tree: InputTree::Provider,
        ..base()
    };
    assert_eq!(put_dir.validate(), Ok(()));
    let manifests = Config {
        task_out: TaskOut::Presigned,
        input_tree: InputTree::Manifests,
        ..base()
    };
    assert_eq!(manifests.validate(), Ok(()));
}

/// Catches a network setting reaching the job as another level, or as the unspecified level
/// the service reads as none: each setting is its own level, the default is the allow-list,
/// and a config of the open network without the opt-in (or the opt-in without it) is refused.
#[test]
fn each_network_setting_is_its_own_job_level() {
    assert_eq!(Network::default(), Network::AllowList);
    assert_eq!(Network::None.level(), NetworkLevel::None);
    assert_eq!(Network::AllowList.level(), NetworkLevel::AllowList);
    assert_eq!(Network::Open.level(), NetworkLevel::Open);
    assert_eq!(base().network, Network::AllowList);
    let open = Config {
        network: Network::Open,
        ..base()
    };
    assert_eq!(open.validate(), Err(ConfigError::OpenNetwork));
    let allowed = Config {
        allow_open_network: true,
        ..open
    };
    assert_eq!(allowed.validate(), Ok(()));
    let unused = Config {
        allow_open_network: true,
        ..base()
    };
    assert_eq!(unused.validate(), Err(ConfigError::OpenNetworkUnused));
}
