//! Fetch, execution and pack beside the other numbers of `compare`, kept per where the phase
//! times came from: measured by the daemon, or claimed by the task. Every time is a number
//! written into a row.

mod common;

use common::*;
use kbf_estimator_replay::compare::{analyze, render_json, render_text};
use kbf_estimator_replay::row::Row;
use serde_json::Value;

const S: u64 = 1_000_000;

/// An attempt with the given provenance (`None`: the page named none) and phase durations in
/// seconds.
fn row(id: &str, provenance: Option<&str>, phases: (Option<u64>, Option<u64>, Option<u64>)) -> Row {
    Row {
        run_id: id.into(),
        phase_times: provenance.map(str::to_owned),
        fetch_us: phases.0.map(|s| s * S),
        execution_us: phases.1.map(|s| s * S),
        pack_us: phases.2.map(|s| s * S),
        ..attempt(id, 0, 1, "ok", None)
    }
}

fn cells<'a>(text: &'a str, label: &str) -> Vec<&'a str> {
    let line = text
        .lines()
        .find(|l| l.len() > 34 && l[..34].trim() == label)
        .unwrap_or_else(|| panic!("no row {label:?} in\n{text}"));
    line[34..].split_whitespace().collect()
}

/// Arm A: two measured attempts (execution 10 and 20 s; the page that named none counts as
/// measured) and one claimed attempt (execution 100 s).
fn arm_a() -> Vec<Row> {
    vec![
        row("a1", Some("measured"), (Some(1), Some(10), Some(2))),
        row("a2", None, (Some(3), Some(20), None)),
        row("a3", Some("task_claim"), (Some(50), Some(100), Some(60))),
    ]
}

/// Arm B: one measured attempt, execution 30 s, and no claimed one.
fn arm_b() -> Vec<Row> {
    vec![row("b1", Some("measured"), (Some(2), Some(30), Some(4)))]
}

/// Catches: claimed and measured phase times pooled into one number (a measured p90 of 20 s
/// would read 100 s, and the claimed p50 would read 20 s), a pooled row printed beside them,
/// a page with no marker kept out of the measured rows, an unknown marker pooled with either
/// kind instead of counted apart, and a kind with no sample given a number (it is `-`).
#[test]
fn claimed_and_measured_rows_are_never_pooled() {
    let mut a = arm_a();
    a.push(row(
        "a4",
        Some("future_kind"),
        (Some(900), Some(900), Some(900)),
    ));
    let text = render_text(&analyze(&a), &analyze(&arm_b()));
    assert_eq!(
        cells(&text, "attempts, phase times measured"),
        ["2", "1", "-1"]
    );
    assert_eq!(
        cells(&text, "attempts, phase times task_claim"),
        ["1", "0", "-1"]
    );
    assert_eq!(
        cells(&text, "attempts, phase times other"),
        ["1", "0", "-1"]
    );
    assert_eq!(
        cells(&text, "execution p50 measured (s)"),
        ["10.000", "30.000", "20.000"]
    );
    assert_eq!(
        cells(&text, "execution p90 measured (s)"),
        ["20.000", "30.000", "10.000"]
    );
    assert_eq!(
        cells(&text, "execution p50 task_claim (s)"),
        ["100.000", "-", "-"]
    );
    assert_eq!(
        cells(&text, "fetch p50 measured (s)"),
        ["1.000", "2.000", "1.000"]
    );
    assert_eq!(
        cells(&text, "fetch p50 task_claim (s)"),
        ["50.000", "-", "-"]
    );
    assert_eq!(
        cells(&text, "pack p50 measured (s)"),
        ["2.000", "4.000", "2.000"]
    );
    assert_eq!(
        cells(&text, "pack p90 task_claim (s)"),
        ["60.000", "-", "-"]
    );
    for pooled in ["execution p50 (s)", "fetch p50 (s)", "pack p50 (s)"] {
        assert!(
            !text.lines().any(|l| l.starts_with(pooled)),
            "a pooled row {pooled:?}: {text}"
        );
    }
    let json: Value = serde_json::from_str(&render_json(&analyze(&a), &analyze(&arm_b()))).unwrap();
    let a = &json["arms"]["a"]["phases"];
    assert_eq!(a["measured"]["rows"], 2);
    assert_eq!(a["measured"]["execution_us"]["p90"], 20 * S);
    assert_eq!(a["task_claim"]["rows"], 1);
    assert_eq!(a["task_claim"]["execution_us"]["p50"], 100 * S);
    assert_eq!(a["other"], 1);
    let b = &json["arms"]["b"]["phases"];
    assert_eq!(b["task_claim"]["execution_us"]["p50"], Value::Null);
    let d = &json["difference"]["phases"];
    assert_eq!(d["measured"]["execution_us"]["p50"], 20 * S);
    assert_eq!(d["task_claim"]["execution_us"]["p50"], Value::Null);
}

/// Catches: a row with a provenance but no phase times counted as a sample of zero, and a
/// phase that one row lacks and the next has dropping the next.
#[test]
fn a_row_without_a_phase_adds_no_sample_to_it() {
    let rows = [
        row("r1", Some("measured"), (None, Some(4), None)),
        row("r2", Some("measured"), (Some(6), None, None)),
    ];
    let text = render_text(&analyze(&rows), &analyze(&[]));
    assert_eq!(
        cells(&text, "execution p50 measured (s)"),
        ["4.000", "-", "-"]
    );
    assert_eq!(cells(&text, "fetch p50 measured (s)"), ["6.000", "-", "-"]);
    assert_eq!(cells(&text, "pack p50 measured (s)"), ["-", "-", "-"]);
}
