//! The example unit of the `tierstore` binary (`packaging/tierstore`) against the binary's flags:
//! the command line parses as the binary parses it, no secret is on it, the stop timeout covers
//! the drain, nothing starts the unit by itself, and it runs as the account that the sysusers.d
//! entry makes.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use tierstore::server::cli::Cli;

/// The words in the name of a flag that say it holds a secret.
const SECRET_WORDS: [&str; 5] = ["key", "secret", "credential", "token", "password"];

/// The directory of the example files.
fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/tierstore")
}

/// The text of a file of the directory.
fn read(file: &str) -> String {
    std::fs::read_to_string(dir().join(file)).unwrap_or_else(|e| panic!("{file}: {e}"))
}

/// The values of the `key=` lines of a unit, its `\` continuations joined.
fn values(unit: &str, key: &str) -> Vec<String> {
    let joined = unit.replace("\\\n", " ");
    let prefix = format!("{key}=");
    joined
        .lines()
        .filter_map(|line| line.strip_prefix(prefix.as_str()))
        .map(str::to_owned)
        .collect()
}

/// The value of the one `key=` line of a unit.
fn value(unit: &str, key: &str) -> String {
    let mut found = values(unit, key);
    assert_eq!(found.len(), 1, "{key} appears once: {found:?}");
    found.remove(0)
}

/// The words of the unit's `ExecStart=`.
fn exec_start(unit: &str) -> Vec<String> {
    value(unit, "ExecStart")
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// The unit's command line, parsed as the binary parses its flags (the program name first).
fn parse(unit: &str) -> Cli {
    Cli::try_parse_from(exec_start(unit)).expect("the unit's flags")
}

/// Asserts that the command line of the unit runs the binary of the release as `tierstore`, that
/// it parses with the flags the binary has, and that it describes two different S3 tiers, each
/// with an endpoint, and four key files of absolute paths that are not the same file: the full
/// key, one read-only key (the daemons') and the key of each tier.
#[test]
fn the_command_line_is_the_binarys_flags_for_two_s3_tiers() {
    let unit = read("tierstore.service");
    assert_eq!(exec_start(&unit)[0], "/usr/libexec/kbf/tierstore");
    let cli = parse(&unit);
    assert!(cli.hot.starts_with("s3:"), "{}", cli.hot);
    assert!(cli.cold.starts_with("s3:"), "{}", cli.cold);
    assert_ne!(cli.hot, cli.cold);
    for endpoint in [&cli.hot_endpoint, &cli.cold_endpoint] {
        let endpoint = endpoint.as_deref().expect("an endpoint");
        assert!(endpoint.starts_with("http://"), "{endpoint}");
    }
    let hot_key = cli.hot_credentials_file.as_deref();
    let hot_key = hot_key.expect("--hot-credentials-file");
    let cold_key = cli.cold_credentials_file.as_deref();
    let cold_key = cold_key.expect("--cold-credentials-file");
    let readers = &cli.read_only_key_file;
    assert_eq!(readers.len(), 1, "{readers:?}");
    let read_only_key = readers[0].as_path();
    let files = [
        cli.access_key_file.as_path(),
        read_only_key,
        hot_key,
        cold_key,
    ];
    for file in files {
        assert!(file.is_absolute(), "{file:?}");
    }
    let distinct: BTreeSet<&Path> = files.into_iter().collect();
    assert_eq!(distinct.len(), 4);
}

/// Asserts that no secret is on the command line or in the environment of the unit: the flags
/// whose names speak of a key, a secret, a credential, a token or a password are the four that
/// name a file (`-file`), and the unit sets no environment variable and reads no environment
/// file.
#[test]
fn no_secret_is_on_the_command_line_or_in_the_environment() {
    let unit = read("tierstore.service");
    let flags: Vec<String> = exec_start(&unit)
        .into_iter()
        .filter(|word| word.starts_with("--"))
        .collect();
    let secret_flags: Vec<&String> = flags
        .iter()
        .filter(|flag| SECRET_WORDS.iter().any(|name| flag.contains(*name)))
        .collect();
    assert_eq!(secret_flags.len(), 4, "{secret_flags:?}");
    for flag in secret_flags {
        assert!(flag.ends_with("-file"), "{flag} is not a file");
    }
    let lines = unit.lines();
    let environment = lines.filter(|line| line.starts_with("Environment"));
    assert_eq!(environment.count(), 0);
}

/// Asserts that systemd stops the unit with SIGTERM, which the binary drains on, after a timeout
/// that is no shorter than the wait of the load balancers plus the deadline of the drain plus the
/// 5 s the reconciler's lease release may take (`STOP_WAIT` in `cli.rs`), so that a drain in
/// progress is not cut by SIGKILL; and that a start that fails is tried again, except when the
/// argument parser refused the flags (exit status 2) or a tier answered the start-up self-test
/// wrongly (exit status 3), which a restart cannot mend.
#[test]
fn the_stop_timeout_covers_the_drain_and_a_refused_command_line_is_not_restarted() {
    let unit = read("tierstore.service");
    let cli = parse(&unit);
    assert_eq!(value(&unit, "KillSignal"), "SIGTERM");
    let seconds: u64 = value(&unit, "TimeoutStopSec").parse().expect("seconds");
    let drain = cli.drain_lb_wait + cli.drain_deadline + Duration::from_secs(5);
    let timeout = Duration::from_secs(seconds);
    assert!(timeout >= drain, "{timeout:?} is less than {drain:?}");
    assert_eq!(value(&unit, "Restart"), "on-failure");
    assert_eq!(value(&unit, "RestartPreventExitStatus"), "2 3");
    // A start that fails with exit 1 for ever (a store that answers 500 to a conditional put) is
    // given up on after 5 starts in 10 minutes, not retried every 5 s for ever.
    assert_eq!(value(&unit, "StartLimitIntervalSec"), "600");
    assert_eq!(value(&unit, "StartLimitBurst"), "5");
}

/// Asserts that placing the unit starts nothing: it has no `[Install]` section, nothing in it
/// makes another unit want or require it, and it is skipped until the access key file, which
/// its command line names, exists.
#[test]
fn the_unit_is_not_enabled_by_placing_it() {
    let unit = read("tierstore.service");
    let sections = unit.lines().filter(|line| line.trim() == "[Install]");
    assert_eq!(sections.count(), 0);
    for key in ["WantedBy", "RequiredBy", "Alias"] {
        assert_eq!(values(&unit, key), Vec::<String>::new(), "{key}");
    }
    let access_key = parse(&unit).access_key_file;
    let condition = value(&unit, "ConditionPathExists");
    assert_eq!(PathBuf::from(condition), access_key);
}

/// Asserts that the unit runs as an account that is not root, that the sysusers.d entry makes
/// exactly that one account, and that it has no login shell.
#[test]
fn the_unit_runs_as_the_account_the_sysusers_entry_makes() {
    let unit = read("tierstore.service");
    let user = value(&unit, "User");
    assert_ne!(user, "root");
    let sysusers = read("tierstore.conf");
    let entries: Vec<&str> = sysusers
        .lines()
        .filter(|line| line.starts_with("u "))
        .collect();
    assert_eq!(entries.len(), 1, "{entries:?}");
    let words: Vec<&str> = entries[0].split_whitespace().collect();
    assert_eq!(words[1], user);
    assert_eq!(words.last(), Some(&"/usr/sbin/nologin"));
}

/// Asserts that the unit keeps the hardening of the repository's other example units that this
/// process can run under: no new privileges, a read-only file system, a private `/tmp`, and no
/// capability.
#[test]
fn the_unit_is_hardened() {
    let unit = read("tierstore.service");
    let wanted = [
        ("NoNewPrivileges", "yes"),
        ("ProtectSystem", "strict"),
        ("PrivateTmp", "yes"),
        ("CapabilityBoundingSet", ""),
    ];
    for (key, want) in wanted {
        assert_eq!(value(&unit, key), want, "{key}");
    }
}
