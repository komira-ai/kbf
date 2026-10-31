//! From the command line to the settings: the flags, and the file `--config` names.

use std::ffi::OsString;
use std::time::Duration;

use clap::Parser;
use kbf_testkit::dir::TestDir;

use super::{Launch, Refused, resolve};
use crate::server::cli::Cli;
use crate::server::config::from_text;
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

/// The refusal of a command line that has none of settings: which kind it is, whether the
/// parser would write it to stderr, and its text.
fn refused(args: &[&str]) -> (&'static str, bool, String) {
    match resolve(words(args)).unwrap_err() {
        Refused::Usage(usage) => ("usage", usage.use_stderr(), usage.to_string()),
        Refused::Config(why) => ("config", true, why.to_string()),
    }
}

/// Asserts that without `--config` the flags are parsed as the parser parses them, and
/// `--print-config` comes through with them.
#[test]
fn flags_without_a_file_are_the_flags() {
    let Launch {
        settings,
        from_file,
        check_only: _,
    } = resolve(words(&golden_flags())).unwrap();
    assert!(!from_file);
    assert_eq!(
        settings.into_storage(),
        Some(Cli::try_parse_from(golden_flags()).unwrap())
    );
    let mut printing = golden_flags();
    printing.push("--print-config");
    let launch = resolve(words(&printing)).unwrap();
    assert!(!launch.from_file);
    assert!(launch.settings.into_storage().unwrap().print_config);
}

/// Asserts that with `--config` the settings are the file's, with the two parts of the drain from
/// the flags when they are given and their defaults when not.
#[test]
fn a_file_makes_the_settings_and_the_drain_stays_a_flag() {
    let (_dir, path) = with_file("tierstore-launch-file");
    let launch = resolve(words(&["tierstore", "--config", &path])).unwrap();
    assert!(launch.from_file);
    assert_eq!(
        launch.settings.into_storage(),
        Some(from_text(GOLDEN, DEFAULTS).unwrap())
    );
    let given = [
        "tierstore",
        "--drain-lb-wait",
        "2s",
        "--config",
        &path,
        "--drain-deadline",
        "4m",
    ];
    let cli = resolve(words(&given))
        .unwrap()
        .settings
        .into_storage()
        .unwrap();
    assert_eq!(cli.drain_lb_wait, Duration::from_secs(2));
    assert_eq!(cli.drain_deadline, Duration::from_secs(240));
    let mut expected = from_text(GOLDEN, DEFAULTS).unwrap();
    expected.drain_lb_wait = cli.drain_lb_wait;
    expected.drain_deadline = cli.drain_deadline;
    assert_eq!(cli, expected);
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
    let (kind, _, text) = refused(&given);
    assert_eq!(kind, "config");
    assert!(
        text.contains("cannot be given with --listen, --hot, --skip-backend-check:"),
        "{text}"
    );
    assert!(!text.contains("--drain-lb-wait,"), "{text}");
}

/// Asserts that `--oci-listen` with `--config` is refused as a conflict that names the flag, even
/// with the address the file's `[oci]` has, and that the file alone makes the front's settings.
#[test]
fn an_oci_flag_with_a_file_conflicts_and_the_file_holds_the_front() {
    let (_dir, path) = with_file("tierstore-launch-oci");
    for address in ["192.0.2.10:5000", "127.0.0.1:1"] {
        let (kind, _, text) = refused(&["tierstore", "--config", &path, "--oci-listen", address]);
        assert_eq!(kind, "config", "{address}");
        assert!(
            text.contains("cannot be given with --oci-listen:"),
            "{address}: {text}"
        );
    }
    let launch = resolve(words(&["tierstore", "--config", &path])).unwrap();
    let cli = launch.settings.into_storage().unwrap();
    assert_eq!(cli.oci_listen, vec!["192.0.2.10:5000".parse().unwrap()]);
    assert_eq!(cli.oci_max_connections, Some(16));
}

/// Asserts that a flag that has a default still conflicts with the file when it is given, even
/// with the default's own value (`--region us-east-1`), and so does a flag that may be repeated,
/// and that each of them is named alone.
#[test]
fn a_flag_given_with_its_default_value_still_conflicts_with_a_file() {
    let (_dir, path) = with_file("tierstore-launch-default");
    let cases = [
        ["--region", "us-east-1", "--region"],
        ["--critical", "95", "--critical"],
        ["--read-only-key-file", "/k", "--read-only-key-file"],
        ["--max-concurrent-puts", "32", "--max-concurrent-puts"],
    ];
    for [flag, value, named] in cases {
        let (kind, _, text) = refused(&["tierstore", "--config", &path, flag, value]);
        assert_eq!(kind, "config", "{flag}");
        assert!(
            text.contains(&format!("cannot be given with {named}:")),
            "{flag}: {text}"
        );
    }
}

/// Asserts that `--print-config` writes the file for flags and is refused with `--config`, and
/// that a file that is not there is a refusal that names it.
#[test]
fn print_config_is_refused_with_a_file_and_a_missing_file_is_named() {
    let (_dir, path) = with_file("tierstore-launch-print");
    let (kind, _, why) = refused(&["tierstore", "--config", &path, "--print-config"]);
    assert_eq!(kind, "config");
    assert!(why.contains("do not give it with --config"), "{why}");
    let (kind, _, why) = refused(&["tierstore", "--config", "/no/such/tierstore.toml"]);
    assert_eq!(kind, "config");
    assert!(why.contains("reading the config file"), "{why}");
    assert!(why.contains("/no/such/tierstore.toml"), "{why}");
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
        let (kind, stderr, _) = refused(&args);
        assert_eq!(kind, "usage", "{args:?}");
        assert!(!stderr, "{args:?}");
    }
    let wrong = [
        vec!["tierstore"],
        vec!["tierstore", "--no-such-flag"],
        vec!["tierstore", "--config", &path, "--no-such-flag"],
        vec!["tierstore", "--config", &path, "--drain-lb-wait", "soon"],
    ];
    for args in wrong {
        let (kind, stderr, _) = refused(&args);
        assert_eq!(kind, "usage", "{args:?}");
        assert!(stderr, "{args:?}");
    }
}
