//! The removed `--job-store-*` flags: each is refused by name, with or without a job listener,
//! and none is in `--help`.

use super::super::store_grant::{REMOVED, REMOVED_WHY};
use crate::frontend::command;

/// The flags of a frontend with a plain job listener, to which a test adds one more.
const BASE: [&str; 8] = [
    "frontend",
    "--storage=http://127.0.0.1:1",
    "--listen=127.0.0.1:0",
    "--worker-listen=127.0.0.1:0",
    "--api-listen=127.0.0.1:0",
    "--job-listen=127.0.0.1:8990",
    "--job-plain-http",
    "--job-grant-secret-file=/etc/kbf/grant.secret",
];

/// Catches: a removed store flag accepted (a frontend that starts as though it handed out store
/// grants), refused with clap's general unknown-argument line, or refused without naming the flag
/// and the release that removed the grants. Each flag is tried with a listener and without one,
/// and with a value it used to take: the refusal is the same whole line every time.
#[test]
fn each_removed_store_flag_is_refused_by_name() {
    let values = [
        ("job-store-grant-key-file", "/etc/kbf/store-grant.key"),
        ("job-store-url", "https://store.farm.test:9000"),
        ("job-store-bucket", "tiers"),
    ];
    assert_eq!(values.map(|(flag, _)| flag), REMOVED);
    for (flag, value) in values {
        let given = format!("--{flag}={value}");
        let with_listener = BASE.iter().copied().chain([given.as_str()]);
        let without = BASE[..2].iter().copied().chain([given.as_str()]);
        for argv in [
            with_listener.collect::<Vec<_>>(),
            without.collect::<Vec<_>>(),
        ] {
            let error = command()
                .try_get_matches_from(&argv)
                .expect_err("a removed flag is refused");
            assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
            let first = error.to_string();
            let first = first.lines().next().expect("a line");
            assert_eq!(
                first,
                format!("error: invalid value '{value}' for '--{flag} <VALUE>': {REMOVED_WHY}")
            );
        }
    }
    assert!(REMOVED_WHY.contains("tierstore v0.3.29"));
}

/// Catches: a removed flag still listed in `--help` as a setting, or dropped from the command
/// (a start that gives it would then get clap's unknown-argument line, not the named refusal).
#[test]
fn the_removed_store_flags_are_in_the_command_and_hidden_from_help() {
    let cmd = command();
    let help = cmd.clone().render_long_help().to_string();
    assert!(!help.contains("job-store"), "{help}");
    for long in REMOVED {
        let arg = cmd.get_arguments().find(|a| a.get_long() == Some(long));
        assert!(arg.is_some_and(clap::Arg::is_hide_set), "{long}");
    }
}
