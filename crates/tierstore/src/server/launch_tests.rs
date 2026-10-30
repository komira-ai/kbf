//! From the command line to the settings: the flags, and the file `--config` names.

use std::ffi::OsString;
use std::time::Duration;

use clap::Parser;
use kbf_testkit::dir::TestDir;

use super::{Launch, Refused, resolve};
use crate::server::cli::Cli;
use crate::server::config::{ConfigError, from_text};
use crate::server::config_testing::{DEFAULTS, GOLDEN, golden_flags};

fn words(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

/// A directory with the golden file in it, and the file's path as a flag value.
fn with_file(name: &str) -> (TestDir, String) {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let path = dir.path().join("tierstore.toml");
    std::fs::write(&path, GOLDEN).unwrap();
    let path = path.display().to_string();
    (dir, path)
}

/// The refusal of a command line that has none of settings.
fn refused(args: &[&str]) -> Refused {
    resolve(words(args)).unwrap_err()
}

/// Asserts that without `--config` the flags are parsed as the parser parses them, and
/// `--print-config` comes through with them.
#[test]
fn flags_without_a_file_are_the_flags() {
    let Launch { cli, from_file } = resolve(words(&golden_flags())).unwrap();
    assert!(!from_file);
    assert_eq!(cli, Cli::try_parse_from(golden_flags()).unwrap());
    let mut printing = golden_flags();
    printing.push("--print-config");
    let launch = resolve(words(&printing)).unwrap();
    assert!(launch.cli.print_config && !launch.from_file);
}

/// Asserts that with `--config` the settings are the file's, with the two parts of the drain from
/// the flags when they are given and their defaults when not.
#[test]
fn a_file_makes_the_settings_and_the_drain_stays_a_flag() {
    let (_dir, path) = with_file("tierstore-launch-file");
    let launch = resolve(words(&["tierstore", "--config", &path])).unwrap();
    assert!(launch.from_file);
    assert_eq!(launch.cli, from_text(GOLDEN, DEFAULTS).unwrap());
    let given = [
        "tierstore",
        "--drain-lb-wait",
        "2s",
        "--config",
        &path,
        "--drain-deadline",
        "4m",
    ];
    let launch = resolve(words(&given)).unwrap();
    assert_eq!(launch.cli.drain_lb_wait, Duration::from_secs(2));
    assert_eq!(launch.cli.drain_deadline, Duration::from_secs(240));
    let mut expected = from_text(GOLDEN, DEFAULTS).unwrap();
    expected.drain_lb_wait = launch.cli.drain_lb_wait;
    expected.drain_deadline = launch.cli.drain_deadline;
    assert_eq!(launch.cli, expected);
}

/// Asserts that a start with `--config` and a flag that describes the data path is refused, with
/// all such flags named and the per-run flags and the defaults not, whatever the order.
#[test]
fn a_file_with_a_data_path_flag_is_refused_naming_the_flags() {
    let (_dir, path) = with_file("tierstore-launch-conflict");
    let given = [
        "tierstore",
        "--listen",
        "127.0.0.1:1",
        "--config",
        &path,
        "--hot",
        "memory",
        "--drain-lb-wait",
        "1s",
        "--skip-backend-check",
        "multipart-conditional=x",
    ];
    let Refused::Config(why) = refused(&given) else {
        panic!("not a config refusal");
    };
    let ConfigError::Conflict { flags } = &why else {
        panic!("{why}");
    };
    assert_eq!(flags, &["--listen", "--hot", "--skip-backend-check"]);
    let text = why.to_string();
    assert!(
        text.contains("--listen, --hot, --skip-backend-check"),
        "{text}"
    );
}

/// Asserts that `--print-config` writes the file for flags and is refused with `--config`, and
/// that a file that is not there is a refusal that names it.
#[test]
fn print_config_is_refused_with_a_file_and_a_missing_file_is_named() {
    let (_dir, path) = with_file("tierstore-launch-print");
    let Refused::Config(why) = refused(&["tierstore", "--config", &path, "--print-config"]) else {
        panic!("not a config refusal");
    };
    assert!(matches!(why, ConfigError::PrintWithConfig), "{why}");
    assert!(why.to_string().contains("--print-config"), "{why}");
    let Refused::Config(why) = refused(&["tierstore", "--config", "/no/such/tierstore.toml"])
    else {
        panic!("not a config refusal");
    };
    assert!(matches!(why, ConfigError::Read { .. }), "{why}");
    assert!(why.to_string().contains("/no/such/tierstore.toml"), "{why}");
}

/// Asserts that the usage and the version are shown (status 0) with or without a file, that no
/// flags, a flag that is not known and an invalid value are refused as usage errors (status 2),
/// and that a refused file is not one of them.
#[test]
fn the_usage_is_the_parsers_with_or_without_a_file() {
    let (_dir, path) = with_file("tierstore-launch-usage");
    let shown = [
        vec!["tierstore", "--help"],
        vec!["tierstore", "--version"],
        vec!["tierstore", "--config", &path, "--help"],
    ];
    for args in shown {
        let Refused::Usage(usage) = refused(&args) else {
            panic!("{args:?}");
        };
        assert!(!usage.use_stderr(), "{args:?}");
    }
    let wrong = [
        vec!["tierstore"],
        vec!["tierstore", "--no-such-flag"],
        vec!["tierstore", "--config", &path, "--no-such-flag"],
        vec!["tierstore", "--config", &path, "--drain-lb-wait", "soon"],
    ];
    for args in wrong {
        let Refused::Usage(usage) = refused(&args) else {
            panic!("{args:?}");
        };
        assert!(usage.use_stderr(), "{args:?}");
    }
}
