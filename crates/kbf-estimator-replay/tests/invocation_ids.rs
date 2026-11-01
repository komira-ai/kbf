//! The invocation ids on a row: read from the run route, kept through the file, and
//! tolerated by readers that predate them.

mod common;

use common::*;
use kbf_estimator_replay::export::{Window, export};
use kbf_estimator_replay::readonly::ReadOnly;
use kbf_estimator_replay::row::Row;

const OLD_LINE: &str = r#"{"run_id":"x","instance":"i","platform":"linux","node":"n","shape":"-","cfg":"-","digest":"-","outcome":"ok","memory_kill":"none","queued_us":7}"#;

/// Catches: a row that drops the ids the run route names (the keys misspelled), or gives
/// one a run that names none (an empty string, or the walk's invocation put in).
#[test]
fn a_run_that_names_its_invocation_and_build_keeps_both() {
    let mut routes = farm();
    let mut named = run(
        "r1",
        "n1",
        OCT_1 + 5_000_000,
        "ok",
        usage(3 * GIB, 60_000_000),
    );
    named["invocation_id"] = "inv-1".into();
    named["correlated_invocations_id"] = "corr-1".into();
    put(&mut routes, "/v1/runs/r1", &named);
    let mut tool_only = run("r2", "n1", OCT_1, "ok", usage(GIB, 1));
    tool_only["invocation_id"] = "inv-2".into();
    put(&mut routes, "/v1/runs/r2", &tool_only);
    let fake = Fake::new(routes);
    let window = Window::parse(SINCE, UNTIL).unwrap();
    let done = export(&ReadOnly::new(&fake), window).unwrap();
    let ids = |id: &str| {
        let r = done.rows.iter().find(|r| r.run_id == id).unwrap();
        (r.invocation_id.clone(), r.correlated_invocations_id.clone())
    };
    assert_eq!(ids("r1"), (Some("inv-1".into()), Some("corr-1".into())));
    assert_eq!(ids("r2"), (Some("inv-2".into()), None));
    assert_eq!(ids("r3"), (None, None), "a run that names none has none");
}

/// Catches: a field that does not survive the file (written under another name, or lost
/// on read), an absent id written as `null` or an empty string (an older replay would
/// read it as an id), and an old line failing to parse.
#[test]
fn rows_with_and_without_the_ids_round_trip_and_old_lines_read_absent() {
    let old = Row::parse(OLD_LINE).unwrap();
    assert_eq!(
        (&old.invocation_id, &old.correlated_invocations_id),
        (&None, &None)
    );
    assert_eq!(Row::parse(&old.line()).unwrap(), old);
    assert!(!old.line().contains("invocation"), "absent is not written");

    let named = Row {
        invocation_id: Some("inv-1".into()),
        correlated_invocations_id: Some("corr-1".into()),
        ..old.clone()
    };
    let line = named.line();
    assert!(line.contains(r#""invocation_id":"inv-1""#), "{line}");
    assert!(
        line.contains(r#""correlated_invocations_id":"corr-1""#),
        "{line}"
    );
    assert_eq!(Row::parse(&line).unwrap(), named);

    let tool_only = Row {
        invocation_id: Some("inv-1".into()),
        ..old
    };
    assert_eq!(Row::parse(&tool_only.line()).unwrap(), tool_only);
}

/// Catches: a reader that refuses a line with a key it does not know, which is how an
/// older replay reads this export.
#[test]
fn a_line_with_a_key_the_reader_does_not_know_still_parses() {
    let line = OLD_LINE.replace(r#""queued_us":7"#, r#""queued_us":7,"future":1"#);
    assert_eq!(Row::parse(&line).unwrap().queued_us, 7);
}

/// Catches: the lease dropped from the row (a joined operation's copies could not be told
/// from retries), a run that names none given one, and the column lost in the file.
#[test]
fn a_run_keeps_its_lease_and_the_row_round_trips_it() {
    let mut routes = farm();
    let mut leased = run("r1", "n1", OCT_1 + 5_000_000, "ok", usage(GIB, 60_000_000));
    leased["lease"] = "3.11".into();
    put(&mut routes, "/v1/runs/r1", &leased);
    let fake = Fake::new(routes);
    let window = Window::parse(SINCE, UNTIL).unwrap();
    let done = export(&ReadOnly::new(&fake), window).unwrap();
    let lease = |id: &str| {
        done.rows
            .iter()
            .find(|r| r.run_id == id)
            .unwrap()
            .lease
            .clone()
    };
    assert_eq!(lease("r1").as_deref(), Some("3.11"));
    assert_eq!(lease("r3"), None);
    let old = Row::parse(OLD_LINE).unwrap();
    assert!(!old.line().contains("lease"), "absent is not written");
    let full = Row {
        lease: Some("3.11".into()),
        ..old
    };
    assert_eq!(Row::parse(&full.line()).unwrap(), full);
}
