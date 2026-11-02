//! Where a row's phase times came from, and the phase durations it keeps: read from the
//! run route, kept through the file, tolerated by readers that predate them.

mod common;

use common::*;
use kbf_estimator_replay::export::{Window, export};
use kbf_estimator_replay::readonly::ReadOnly;
use kbf_estimator_replay::row::Row;
use serde_json::json;

const OLD_LINE: &str = r#"{"run_id":"x","instance":"i","platform":"linux","node":"n","shape":"-","cfg":"-","digest":"-","outcome":"ok","memory_kill":"none","queued_us":7}"#;

/// Catches: the provenance dropped from the row (a claimed phase pooled with measured ones),
/// a run page that has no marker given one (it is read as measured by the compare, and the
/// row says nothing), a phase duration taken from the wrong pair of times, and a phase
/// whose end is missing given a duration.
#[test]
fn a_run_keeps_its_phase_provenance_and_the_phase_durations() {
    let mut routes = farm();
    let mut claimed = run("r1", "n1", OCT_1 + 5_000_000, "ok", usage(GIB, 60_000_000));
    claimed["phase_times"] = "task_claim".into();
    claimed["reapi_times"] = json!({
        "input_fetch_start": at(OCT_1 + 6_000_000),
        "input_fetch_completed": at(OCT_1 + 8_000_000),
        "execution_start": at(OCT_1 + 8_000_000),
        "execution_completed": at(OCT_1 + 38_000_000),
        "output_upload_start": at(OCT_1 + 38_000_000),
    });
    put(&mut routes, "/v1/runs/r1", &claimed);
    let mut measured = run("r2", "n1", OCT_1, "ok", usage(GIB, 1));
    measured["phase_times"] = "measured".into();
    put(&mut routes, "/v1/runs/r2", &measured);
    let fake = Fake::new(routes);
    let window = Window::parse(SINCE, UNTIL).unwrap();
    let done = export(&ReadOnly::new(&fake), window).unwrap();
    let get = |id: &str| done.rows.iter().find(|r| r.run_id == id).unwrap();
    let r1 = get("r1");
    assert_eq!(r1.phase_times.as_deref(), Some("task_claim"));
    assert_eq!(r1.fetch_us, Some(2_000_000));
    assert_eq!(r1.execution_us, Some(30_000_000));
    assert_eq!(r1.pack_us, None, "no end of the upload, no duration");
    assert_eq!(get("r2").phase_times.as_deref(), Some("measured"));
    let r3 = get("r3");
    assert_eq!(r3.phase_times, None, "an old page names none");
    assert_eq!(
        (r3.fetch_us, r3.execution_us, r3.pack_us),
        (None, None, None)
    );
}

/// Catches: the new columns lost on the way through the file, written for a row that has
/// none (an older replay would read an empty string), and an old line failing to parse.
#[test]
fn rows_with_and_without_the_phase_columns_round_trip() {
    let old = Row::parse(OLD_LINE).unwrap();
    assert_eq!(old.phase_times, None);
    assert!(!old.line().contains("phase_times"), "absent is not written");
    assert!(!old.line().contains("fetch_us"));
    assert_eq!(Row::parse(&old.line()).unwrap(), old);
    let full = Row {
        phase_times: Some("task_claim".into()),
        fetch_us: Some(1),
        execution_us: Some(2),
        pack_us: Some(3),
        ..old
    };
    let line = full.line();
    assert!(line.contains(r#""phase_times":"task_claim""#), "{line}");
    assert_eq!(Row::parse(&line).unwrap(), full);
}

/// Catches: a phase whose end is before its start given a wrapped (huge) duration, and a
/// time that does not parse passed over (the walk would invent a row from half a page): a
/// bad start and a bad end each fail the export, and name the page.
#[test]
fn phase_times_out_of_order_have_no_duration_and_unreadable_ones_fail_the_walk() {
    let window = || Window::parse(SINCE, UNTIL).unwrap();
    let with = |times: serde_json::Value| {
        let mut routes = farm();
        let mut r = run("r1", "n1", OCT_1 + 5_000_000, "ok", usage(GIB, 60_000_000));
        r["reapi_times"] = times;
        put(&mut routes, "/v1/runs/r1", &r);
        Fake::new(routes)
    };
    let backwards = with(json!({
        "execution_start": at(OCT_1 + 9_000_000),
        "execution_completed": at(OCT_1 + 8_000_000),
    }));
    let done = export(&ReadOnly::new(&backwards), window()).unwrap();
    let r1 = done.rows.iter().find(|r| r.run_id == "r1").unwrap();
    assert_eq!(r1.execution_us, None);
    for bad in [
        json!({"execution_start": "soon", "execution_completed": at(OCT_1)}),
        json!({"execution_start": at(OCT_1), "execution_completed": "later"}),
    ] {
        let fake = with(bad);
        assert!(export(&ReadOnly::new(&fake), window()).is_err());
    }
}
