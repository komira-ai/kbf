//! The state-store check: a frontend is refused without a state store, and no other mode
//! is asked for one. Each test goes from the flags to the report.

use super::state_store::check;
use super::tests::DAEMON;
use super::{Class, REFUSED, Report};
use crate::Invocation;

/// The invocation of `kbf` run with `args`.
fn parsed(args: &[&str]) -> Invocation {
    let argv = std::iter::once("kbf").chain(args.iter().copied());
    crate::parse_from(argv).expect("flags parse")
}

/// The invocation of `kbf frontend` run with `more` beside the one flag it requires.
fn frontend(more: &[&str]) -> Invocation {
    let mut args = vec!["frontend", "--storage=http://127.0.0.1:8983"];
    args.extend_from_slice(more);
    parsed(&args)
}

/// What the check finds in `invocation`.
fn found(invocation: &Invocation) -> Report {
    let mut report = Report::default();
    check(invocation, &mut report);
    report
}

/// Asserts that a frontend given no state store is refused with exit status 2 and one
/// problem that names the flag, says why and gives the fix, and that the problem is the
/// only one.
#[test]
fn a_frontend_with_no_state_store_is_refused_naming_the_flag() {
    let report = found(&frontend(&[]));
    assert_eq!(report.exit_status(), Some(REFUSED));
    assert_eq!(report.problems.len(), 1, "{report:#?}");
    assert_eq!(report.problems[0].class, Class::Refused);
    let line = report.render("frontend");
    for word in [
        "kbf frontend: --state-store is not set (",
        "Postgres",
        "--state-store=postgres:///kbf?host=/run/postgresql",
        "state-store key of kbf.toml",
    ] {
        assert!(line.contains(word), "{word} not in {line}");
    }
}

/// Asserts that a frontend given a state store of either kind, Postgres or SQLite, passes
/// the check with nothing to report.
#[test]
fn a_frontend_with_a_state_store_passes() {
    for url in [
        "postgres:///kbf?host=/run/postgresql",
        "sqlite:///var/lib/kbf/state.db",
    ] {
        let flag = format!("--state-store={url}");
        assert_eq!(
            found(&frontend(&[flag.as_str()])),
            Report::default(),
            "{url}"
        );
    }
}

/// Asserts that a build node and an all-in-one, which is given the local Postgres when it
/// names no store, are not asked for a state store.
#[test]
fn the_other_modes_are_not_asked_for_a_state_store() {
    let mut args = vec!["daemon"];
    args.extend_from_slice(&DAEMON);
    assert_eq!(found(&parsed(&args)), Report::default());
    let mut args = vec!["all-in-one", "--meta-dir=/m"];
    args.extend_from_slice(&DAEMON);
    assert_eq!(found(&parsed(&args)), Report::default());
}
