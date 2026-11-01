//! The two-arm comparison beside the per-attempt numbers: per-build numbers in the table
//! and the JSON, the per-run values, the grouping, and the not-recorded list. Every time is
//! a number written into a row.

mod common;

use common::*;
use kbf_estimator_replay::builds::{GroupBy, RunLabels};
use kbf_estimator_replay::compare::{
    NOT_RECORDED, analyze, analyze_grouped, render_json, render_json_runs, render_text,
};
use kbf_estimator_replay::row::Row;
use serde_json::Value;

const S: u64 = 1_000_000;

/// An attempt of invocation `inv` (CI job `corr`) with the given queued, started and ended
/// seconds.
fn row(id: &str, inv: &str, corr: Option<&str>, times: (u64, u64, u64)) -> Row {
    Row {
        run_id: id.into(),
        queued_us: OCT_1 + times.0 * S,
        started_us: Some(OCT_1 + times.1 * S),
        ended_us: Some(OCT_1 + times.2 * S),
        invocation_id: Some(inv.into()),
        correlated_invocations_id: corr.map(str::to_owned),
        ..attempt(id, 0, 1, "ok", None)
    }
}

/// Arm A: invocation `x` (CI job `job`) span 30 s, waits 2 and 4 s; invocation `y` span 10 s,
/// wait 1 s.
fn arm_a() -> Vec<Row> {
    vec![
        row("a1", "x", Some("job"), (0, 2, 12)),
        row("a2", "x", Some("job"), (1, 5, 30)),
        row("a3", "y", None, (100, 101, 110)),
    ]
}

/// Arm B: the same two builds; `x` span 35 s, waits 3 and 5 s; `y` span 15 s, wait 2 s.
fn arm_b() -> Vec<Row> {
    vec![
        row("b1", "x", Some("job"), (0, 3, 17)),
        row("b2", "x", Some("job"), (1, 6, 35)),
        row("b3", "y", None, (100, 102, 115)),
    ]
}

fn without_ids(rows: Vec<Row>) -> Vec<Row> {
    rows.into_iter()
        .map(|r| Row {
            invocation_id: None,
            correlated_invocations_id: None,
            ..r
        })
        .collect()
}

fn cells<'a>(text: &'a str, label: &str) -> Vec<&'a str> {
    let line = text
        .lines()
        .find(|l| l.len() > 34 && l[..34].trim() == label)
        .unwrap_or_else(|| panic!("no row {label:?} in\n{text}"));
    line[34..].split_whitespace().collect()
}

fn labels(arm: &str) -> RunLabels {
    RunLabels {
        setting: "k7".into(),
        arm: arm.into(),
        seed: 3,
        commit: "abc".into(),
    }
}

/// Catches: per-build numbers missing beside the per-attempt ones, a span that is not first
/// queue to last end, a total that is named a wait or not summed (x: 2+4 s, y: 1 s), a
/// within-build percentile taken over all attempts, rank rounding other than nearest rank,
/// and a difference taken A less B.
#[test]
fn the_table_gives_per_build_numbers_beside_the_attempt_numbers() {
    let text = render_text(&analyze(&arm_a()), &analyze(&arm_b()));
    assert_eq!(cells(&text, "builds"), ["2", "2", "0"]);
    assert_eq!(
        cells(&text, "build action span p50 (s)"),
        ["10.000", "15.000", "5.000"]
    );
    assert_eq!(
        cells(&text, "build action span p90 (s)"),
        ["30.000", "35.000", "5.000"]
    );
    assert_eq!(
        cells(&text, "queue time total p50 (attempt-s)"),
        ["1.000", "2.000", "1.000"]
    );
    assert_eq!(
        cells(&text, "queue time total p90 (attempt-s)"),
        ["6.000", "8.000", "2.000"]
    );
    assert_eq!(
        cells(&text, "attempt wait p50 in a build (s)"),
        ["1.000", "2.000", "1.000"]
    );
    assert_eq!(
        cells(&text, "attempt wait p90 in a build (s)"),
        ["1.000", "2.000", "1.000"]
    );
    assert_eq!(cells(&text, "queue wait samples"), ["3", "3", "0"]);
    assert!(!text.contains("build wall"), "{text}");
    assert!(!text.contains("build queue wait"), "{text}");
}

/// Catches: the CI-job view not merging invocations (x's two attempts and a third attempt
/// of another invocation in the same job would be separate builds), and rows with no
/// correlated id put in a build instead of counted.
#[test]
fn grouping_by_correlated_id_gives_the_ci_job_view() {
    let mut a = arm_a();
    a.push(row("a4", "z", Some("job"), (5, 6, 50)));
    let by_job = analyze_grouped(&a, GroupBy::Correlated);
    let text = render_text(&by_job, &analyze_grouped(&arm_b(), GroupBy::Correlated));
    assert_eq!(cells(&text, "builds"), ["1", "1", "0"]);
    assert_eq!(cells(&text, "build rows with no id"), ["1", "1", "0"]);
    assert_eq!(
        cells(&text, "build action span p50 (s)"),
        ["50.000", "35.000", "-15.000"]
    );
    let by_invocation = render_text(&analyze(&a), &analyze(&arm_b()));
    assert_eq!(cells(&by_invocation, "builds"), ["3", "2", "-1"]);
    assert_eq!(
        cells(&by_invocation, "build rows with no id"),
        ["0", "0", "0"]
    );
}

/// Catches: the per-build gap still listed when both arms carry the ids (the report would
/// say the data does not exist), it dropped when an arm has none (a number printed for a
/// build nobody could name), and another gap dropped with it.
#[test]
fn the_per_build_gap_is_listed_only_for_rows_without_the_ids() {
    let gap = NOT_RECORDED[0];
    assert!(gap.starts_with("Per-build"), "{gap}");
    assert!(gap.contains("build action span"), "{gap}");
    assert!(gap.contains("queue time"), "{gap}");
    assert!(
        !gap.contains("wall") && !gap.contains("queue wait"),
        "{gap}"
    );
    let both = render_text(&analyze(&arm_a()), &analyze(&arm_b()));
    assert!(!both.contains(gap), "{both}");
    for kept in &NOT_RECORDED[1..] {
        assert!(both.contains(kept), "{kept}");
    }
    let plain = without_ids(arm_b());
    let one = render_text(&analyze(&arm_a()), &analyze(&plain));
    assert!(one.contains(gap), "{one}");
    assert_eq!(cells(&one, "builds"), ["2", "0", "-2"]);
    assert_eq!(
        cells(&one, "build action span p50 (s)"),
        ["10.000", "-", "-"]
    );
    let neither = render_text(&analyze(&plain), &analyze(&plain));
    assert!(neither.contains(gap), "{neither}");
}

/// Catches: per-build numbers missing from the JSON, a difference of the wrong sign, a gap
/// list that does not follow the data, and a `runs` key written without labels.
#[test]
fn the_json_has_per_build_numbers_and_no_runs_without_labels() {
    let json: Value =
        serde_json::from_str(&render_json(&analyze(&arm_a()), &analyze(&arm_b()))).unwrap();
    let a = &json["arms"]["a"]["builds"];
    assert_eq!(a["count"], 2);
    assert_eq!(a["rows_without_id"], 0);
    assert_eq!(a["action_span_us"]["p50"], 10 * S);
    assert_eq!(a["queue_time_total_us"]["p90"], 6 * S);
    assert_eq!(a["attempt_queue_wait_p50_us"]["p90"], 2 * S);
    assert_eq!(a["attempt_queue_wait_p90_us"]["p90"], 4 * S);
    let b = &json["arms"]["b"]["builds"];
    assert_eq!(b["action_span_us"]["p90"], 35 * S);
    assert_eq!(b["queue_time_total_us"]["p90"], 8 * S);
    let d = &json["difference"]["builds"];
    assert_eq!(d["action_span_us"]["p50"], 5 * S);
    assert_eq!(d["queue_time_total_us"]["p50"], S);
    assert_eq!(d["attempt_queue_wait_p90_us"]["p90"], S);
    assert_eq!(json["not_recorded"].as_array().unwrap().len(), 4);
    assert!(json.get("runs").is_none(), "{json}");
}

/// Catches: per-run values that are not one `RunRecord` per build under the caller's arm
/// label, labels the caller did not give, metrics under old names (`build_wall_us`), and a
/// metric that is zero where the build has no value.
#[test]
fn the_runs_are_run_records_under_the_callers_labels() {
    let (a, b) = (analyze(&arm_a()), analyze(&arm_b()));
    let (la, lb) = (labels("arm-a"), labels("arm-b"));
    let json: Value = serde_json::from_str(&render_json_runs(&a, &b, Some((&la, &lb)))).unwrap();
    for (arm, first_span) in [("a", 30.0e6), ("b", 35.0e6)] {
        let runs = json["runs"][arm].as_array().unwrap();
        assert_eq!(runs.len(), 2, "{arm}");
        for (i, r) in runs.iter().enumerate() {
            let mut keys: Vec<&str> = r.as_object().unwrap().keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                [
                    "arm",
                    "commit",
                    "label",
                    "metrics",
                    "run_index",
                    "seed",
                    "setting"
                ]
            );
            assert_eq!(r["arm"], format!("arm-{arm}"));
            assert_eq!(r["setting"], "k7");
            assert_eq!(r["seed"], 3);
            assert_eq!(r["commit"], "abc");
            assert_eq!(r["run_index"], i);
        }
        assert_eq!(runs[0]["metrics"]["build_action_span_us"], first_span);
        assert!(runs[0]["metrics"].get("build_wall_us").is_none());
    }
    let mut open = arm_a();
    open.push(row("a9", "u", None, (200, 201, 202)));
    open[3].ended_us = None;
    let json: Value =
        serde_json::from_str(&render_json_runs(&analyze(&open), &b, Some((&la, &lb)))).unwrap();
    assert_eq!(
        json["runs"]["a"][2]["metrics"]["build_action_span_us"],
        Value::Null
    );
}

/// Catches: an arm with no ids given numbers (zero) in the JSON where the text says `-`,
/// and a gap list that drops the per-build entry.
#[test]
fn an_arm_with_no_ids_has_null_build_numbers_and_the_full_gap_list() {
    let stats = analyze(&without_ids(arm_a()));
    let json: Value = serde_json::from_str(&render_json(&stats, &stats)).unwrap();
    assert_eq!(json["arms"]["a"]["builds"]["count"], 0);
    assert_eq!(json["arms"]["a"]["builds"]["rows_without_id"], 3);
    assert_eq!(
        json["arms"]["a"]["builds"]["action_span_us"]["p50"],
        Value::Null
    );
    assert_eq!(
        json["difference"]["builds"]["action_span_us"]["p50"],
        Value::Null
    );
    assert_eq!(json["not_recorded"].as_array().unwrap().len(), 5);
}

/// Documents (and pins) today's counting: one operation joined by two invocations is
/// exported as two rows, one per waiter, each with its own run id. Per invocation that is
/// right, as each build waited on it; the arm-level attempt count, the queue time total of
/// the CI-job view and the kill rates count the operation once per waiter until the export
/// carries the operation and they count it once (komira-ai/kbf#2207). When that lands this
/// test changes with it.
#[test]
fn a_joined_operation_is_counted_once_per_waiter_until_the_export_names_it() {
    let copy = |id: &str, inv: &str| Row {
        operation: Some("op-1".into()),
        ..row(id, inv, Some("job"), (0, 2, 12))
    };
    let rows = [copy("c1", "i1"), copy("c2", "i2")];
    let arm = analyze(&rows);
    assert_eq!(arm.attempts, 2, "one attempt, counted for each waiter");
    assert_eq!(arm.builds.runs.len(), 2);
    assert!(arm.builds.runs.iter().all(|r| r.attempts == 1));
    let job = analyze_grouped(&rows, GroupBy::Correlated);
    assert_eq!(job.builds.runs.len(), 1);
    assert_eq!(job.builds.runs[0].attempts, 2);
    assert_eq!(
        job.builds.runs[0].queue_time_total_us,
        Some(4 * S),
        "2 s of queueing, counted twice"
    );
}

/// Catches: the table printing build numbers without saying what the span leaves out.
#[test]
fn the_table_says_the_action_span_is_the_servers() {
    let text = render_text(&analyze(&arm_a()), &analyze(&arm_b()));
    let note = "build action span (server): excludes the client's own analysis and local actions";
    assert!(text.contains(note), "{text}");
    let plain = without_ids(arm_a());
    let none = render_text(&analyze(&plain), &analyze(&plain));
    assert!(!none.contains(note), "no builds, no note: {none}");
}
