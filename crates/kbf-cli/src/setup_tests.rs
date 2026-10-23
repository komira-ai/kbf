//! The verb `kbf setup` ([`super`]) without a host: what a run prints and how it ends, and how
//! the command line is read. The run over a host is `tests/setup.rs`. Each test says the
//! behaviour it asserts.

use std::ffi::OsString;

use clap::CommandFactory as _;
use clap::error::ErrorKind;
use kbf_setup::cli::Mode;
use kbf_setup::exit::Exit;
use kbf_setup::setup::SetupError;

use super::{Args, SETUP, finish, parse};

const STEPS: &[u8] = b"+ mkdir /etc/kbf\nnothing to do\n";

fn words(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

/// A run over the steps `STEPS` that ends as `result`: the exit it stands for, and what it
/// wrote to standard output and to standard error.
fn ended(json: bool, result: Result<(), SetupError>) -> (Exit, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let exit = finish(json, result, STEPS, &mut out, &mut err);
    let text = |bytes: Vec<u8>| String::from_utf8(bytes).expect("text");
    (exit, text(out), text(err))
}

/// Asserts that the command line is valid for clap's own checks (ids, flags and help are
/// consistent), so a mistake in the definition fails here and not on a host.
#[test]
fn the_command_line_definition_is_consistent() {
    Args::command().debug_assert();
}

/// Asserts that the verb word is skipped and the flags after it are read: the mode, the
/// verb's own `--apply` and `--json`, and a flag the library defines.
#[test]
fn the_flags_after_the_verb_word_are_read() {
    let argv = words(&[
        "kbf",
        SETUP,
        "--mode=storage",
        "--apply",
        "--json",
        "--no-start",
    ]);
    let args = parse(&argv).expect("a command line");
    assert_eq!(args.setup.mode, Mode::Storage);
    assert!(args.apply);
    assert!(args.json);
    assert!(args.setup.no_start);
    let argv = words(&["kbf", SETUP, "--mode=daemon"]);
    let plain = parse(&argv).expect("a command line");
    assert!(!plain.apply);
    assert!(!plain.json);
    assert!(!plain.setup.no_start);
}

/// Asserts that a command line without a mode, with a mode `kbf` has not, or with a flag the
/// verb does not take is a usage error: clap's refusal, which ends the program with status 2.
#[test]
fn a_command_line_the_verb_does_not_take_is_refused() {
    let cases: [(&[&str], ErrorKind); 3] = [
        (&[], ErrorKind::MissingRequiredArgument),
        (&["--mode=node"], ErrorKind::InvalidValue),
        (&["--mode=daemon", "--check"], ErrorKind::UnknownArgument),
    ];
    for (rest, kind) in cases {
        let mut argv = vec!["kbf", SETUP];
        argv.extend_from_slice(rest);
        let refused = parse(&words(&argv)).err();
        assert_eq!(refused.map(|e| e.kind()), Some(kind), "{rest:?}");
    }
}

/// Asserts that the proxy's flags are not flags of this verb: each is refused as an unknown
/// flag, because a `kbf` that has them would link the proxy.
#[test]
fn the_proxys_flags_are_not_taken() {
    for flag in [
        "--proxy",
        "--proxy-base-port=18000",
        "--buck2-auth=none",
        "--buck2-token-file=/x",
        "--storage-port=8990",
    ] {
        let argv = words(&["kbf", SETUP, "--mode=frontend", flag]);
        let refused = parse(&argv).err();
        assert_eq!(
            refused.map(|e| e.kind()),
            Some(ErrorKind::UnknownArgument),
            "{flag}"
        );
    }
}

/// Asserts that a run that went well writes its steps to standard output and nothing to standard
/// error, and ends with status 0.
#[test]
fn a_run_that_went_well_prints_its_steps_on_standard_output() {
    let (exit, out, err) = ended(false, Ok(()));
    assert_eq!(exit, Exit::Ok);
    assert_eq!(out.as_bytes(), STEPS);
    assert_eq!(err, "");
}

/// Asserts that with `--json` a run that went well writes its steps to standard error, so
/// that standard output holds only the object of a failure.
#[test]
fn a_run_that_went_well_with_json_prints_its_steps_on_standard_error() {
    let (exit, out, err) = ended(true, Ok(()));
    assert_eq!(exit, Exit::Ok);
    assert_eq!(out, "");
    assert_eq!(err.as_bytes(), STEPS);
}

/// Asserts that a failure prints the steps taken so far and then `kbf setup: ` and the reason on
/// standard error, and ends with the status the failure stands for (2 for a missing prerequisite,
/// 5 when only a person can go on).
#[test]
fn a_failure_prints_the_steps_and_its_reason_and_ends_with_its_status() {
    let (exit, out, err) = ended(false, Err(SetupError::NotRoot { euid: 1000 }));
    assert_eq!(exit, Exit::Usage);
    assert_eq!(out, "");
    let steps = String::from_utf8_lossy(STEPS);
    let why = "kbf setup: setup writes /etc and manages services: run it with sudo";
    assert!(err.starts_with(&format!("{steps}{why}")), "{err}");
    let (exit, _, err) = ended(false, Err(SetupError::NeedsOperator("fix it".to_owned())));
    assert_eq!(exit, Exit::NeedsOperator);
    assert!(err.ends_with("kbf setup: fix it\n"), "{err}");
}

/// Asserts that with `--json` a failure is one object on standard output with the verb, the
/// stable word of the error and its message, and that nothing is written to standard error.
#[test]
fn a_failure_with_json_is_one_object_on_standard_output() {
    let (exit, out, err) = ended(true, Err(SetupError::NeedsOperator("fix it".to_owned())));
    assert_eq!(exit, Exit::NeedsOperator);
    assert_eq!(err, "");
    let object: serde_json::Value = serde_json::from_str(&out).expect("one JSON object");
    assert_eq!(
        object,
        serde_json::json!({"verb": "setup", "error": "needs_operator", "message": "fix it"})
    );
}
