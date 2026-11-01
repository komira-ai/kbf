//! Unit tests of `--whole-node-max` and its tuning flags (`super`).

use clap::Parser;
use clap::error::ErrorKind;
use kbf_sched::WholeNodeConfig;

use crate::Args;

fn parse(args: &[&str]) -> Result<Args, clap::Error> {
    Args::try_parse_from(std::iter::once("kbf-server").chain(args.iter().copied()))
}

fn config(args: &[&str]) -> Option<WholeNodeConfig> {
    let listeners = parse(args).expect("flags parse").listeners();
    listeners.expect("listeners").whole_node
}

/// Catches a default that turns the option on (a server started with no flag would hold
/// workers for whole-machine requests), a tuning flag taken without `--whole-node-max` (it
/// would be ignored in silence), and a value of `--whole-node-max` that is no worker count.
#[test]
fn the_option_is_off_unless_a_ceiling_is_given() {
    assert_eq!(config(&[]), None);
    assert!(config(&["--whole-node-max=3"]).is_some());
    for tuning in [
        "--whole-node-max-draining=2",
        "--whole-node-keep-for-ordinary=0",
        "--whole-node-allow-last-capable",
        "--whole-node-drain-after-secs=5",
        "--whole-node-drain-deadline-secs=5",
        "--whole-node-reserve-backoff-secs=5",
        "--whole-node-max-reserve-attempts=3",
    ] {
        let refused = parse(&[tuning]).expect_err(tuning);
        assert_eq!(
            refused.kind(),
            ErrorKind::MissingRequiredArgument,
            "{tuning}"
        );
        assert!(parse(&["--whole-node-max=1", tuning]).is_ok(), "{tuning}");
    }
    for bad in ["", "0", "-1", "many", "1.5"] {
        let flag = format!("--whole-node-max={bad}");
        let refused = parse(&[&flag]).expect_err(bad);
        assert_eq!(refused.kind(), ErrorKind::ValueValidation, "{bad:?}");
    }
}

/// Catches a default that differs from the scheduler library's for a mixed pool (the
/// settings the design says a farm that also runs ordinary work starts from), a ceiling
/// that does not reach the settings, and a setting the library refuses for being zero.
#[test]
fn the_defaults_are_the_librarys_for_a_mixed_pool() {
    for max in [1, 4, 64] {
        let flag = format!("--whole-node-max={max}");
        let config = config(&[&flag]).expect("on");
        assert_eq!(config, WholeNodeConfig::mixed(max), "{max}");
        assert_eq!(config.zero_key(), None, "{max}");
    }
}

/// Catches a tuning flag that reaches no setting, reaches another one, or is read in the
/// wrong unit (every time flag is in seconds, the setting in milliseconds), and a setting
/// that changes without a flag naming it.
#[test]
fn each_flag_reaches_its_own_setting() {
    let base = WholeNodeConfig::mixed(5);
    let with = |flag: &str| config(&["--whole-node-max=5", flag]).expect("on");
    assert_eq!(
        with("--whole-node-max-draining=3"),
        WholeNodeConfig {
            max_draining: 3,
            ..base.clone()
        }
    );
    assert_eq!(
        with("--whole-node-keep-for-ordinary=0"),
        WholeNodeConfig {
            keep_for_ordinary: 0,
            ..base.clone()
        }
    );
    assert_eq!(
        with("--whole-node-allow-last-capable"),
        WholeNodeConfig {
            allow_last_capable: true,
            ..base.clone()
        }
    );
    assert_eq!(
        with("--whole-node-drain-after-secs=30"),
        WholeNodeConfig {
            drain_after_ms: 30_000,
            ..base.clone()
        }
    );
    assert_eq!(
        with("--whole-node-drain-deadline-secs=120"),
        WholeNodeConfig {
            drain_deadline_ms: Some(120_000),
            ..base.clone()
        }
    );
    assert_eq!(
        with("--whole-node-reserve-backoff-secs=60"),
        WholeNodeConfig {
            reserve_backoff_ms: 60_000,
            ..base.clone()
        }
    );
    assert_eq!(
        with("--whole-node-max-reserve-attempts=7"),
        WholeNodeConfig {
            max_reserve_attempts: 7,
            ..base
        }
    );
}

/// Catches a setting that must be at least 1 accepting 0 (the library would divide its
/// bounds by nothing or never reserve), a seconds value that overflows milliseconds
/// wrapping to a short wait, and a keep-for-ordinary of 0 refused (it is the setting of a
/// dedicated pool).
#[test]
fn zero_is_refused_where_it_means_nothing() {
    for zero in [
        "--whole-node-max-draining=0",
        "--whole-node-drain-deadline-secs=0",
        "--whole-node-reserve-backoff-secs=0",
        "--whole-node-max-reserve-attempts=0",
    ] {
        let refused = parse(&["--whole-node-max=1", zero]).expect_err(zero);
        assert_eq!(refused.kind(), ErrorKind::ValueValidation, "{zero}");
    }
    let huge = format!("--whole-node-drain-after-secs={}", u64::MAX);
    let wait = config(&["--whole-node-max=1", &huge]).expect("on");
    assert_eq!(wait.drain_after_ms, u64::MAX);
    assert_eq!(
        config(&["--whole-node-max=1", "--whole-node-keep-for-ordinary=0"])
            .expect("on")
            .keep_for_ordinary,
        0
    );
}
