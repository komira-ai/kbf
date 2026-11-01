//! The two-arm comparison beside the per-attempt numbers: per-build wall and queue wait in
//! the table and the JSON, the per-run values, and the not-recorded list. Every time is a
//! number written into a row.

mod common;

use common::*;
use kbf_estimator_replay::compare::{NOT_RECORDED, analyze, render_json, render_text};
use kbf_estimator_replay::row::Row;
use serde_json::Value;

const S: u64 = 1_000_000;

/// An attempt of invocation `inv` (build `corr`) with the given queued, started and ended
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

/// Arm A: build `c1` (two invocations) wall 30 s, queue wait 2+4 s; build `i2` wall 10 s,
/// wait 1 s.
fn arm_a() -> Vec<Row> {
    vec![
        row("a1", "x", Some("c1"), (0, 2, 12)),
        row("a2", "y", Some("c1"), (1, 5, 30)),
        row("a3", "i2", None, (100, 101, 110)),
    ]
}

/// Arm B: the same two builds, each 5 s longer in wall; `c1` waits 3+5 s, `i2` 2 s.
fn arm_b() -> Vec<Row> {
    vec![
        row("b1", "x", Some("c1"), (0, 3, 17)),
        row("b2", "y", Some("c1"), (1, 6, 35)),
        row("b3", "i2", None, (100, 102, 115)),
    ]
}

fn cells<'a>(text: &'a str, label: &str) -> Vec<&'a str> {
    let line = text
        .lines()
        .find(|l| l.len() > 34 && l[..34].trim() == label)
        .unwrap_or_else(|| panic!("no row {label:?} in\n{text}"));
    line[34..].split_whitespace().collect()
}

/// Catches: per-build numbers missing beside the per-attempt ones, a build wall that is not
/// first queue to last end, rank rounding other than nearest rank (two builds: p50 is the
/// smaller, p90 the larger), and a difference taken A less B.
#[test]
fn the_table_gives_per_build_wall_and_queue_wait_beside_the_attempt_numbers() {
    let text = render_text(&analyze(&arm_a()), &analyze(&arm_b()));
    assert_eq!(cells(&text, "builds"), ["2", "2", "0"]);
    assert_eq!(
        cells(&text, "build wall p50 (s)"),
        ["10.000", "15.000", "5.000"]
    );
    assert_eq!(
        cells(&text, "build wall p90 (s)"),
        ["30.000", "35.000", "5.000"]
    );
    assert_eq!(
        cells(&text, "build queue wait p50 (s)"),
        ["1.000", "2.000", "1.000"]
    );
    assert_eq!(
        cells(&text, "build queue wait p90 (s)"),
        ["6.000", "8.000", "2.000"]
    );
    assert_eq!(cells(&text, "queue wait samples"), ["3", "3", "0"]);
}

/// Catches: the per-build gap still listed when both arms carry the ids (the report would
/// say the data does not exist), it dropped when an arm has none (a number printed for a
/// build nobody could name), and another gap dropped with it.
#[test]
fn the_per_build_gap_is_listed_only_for_rows_without_the_ids() {
    let gap = NOT_RECORDED[0];
    assert!(gap.starts_with("Per-build"), "{gap}");
    let both = render_text(&analyze(&arm_a()), &analyze(&arm_b()));
    assert!(!both.contains(gap), "{both}");
    for kept in &NOT_RECORDED[1..] {
        assert!(both.contains(kept), "{kept}");
    }
    let no_ids: Vec<Row> = arm_b()
        .into_iter()
        .map(|r| Row {
            invocation_id: None,
            correlated_invocations_id: None,
            ..r
        })
        .collect();
    let one = render_text(&analyze(&arm_a()), &analyze(&no_ids));
    assert!(one.contains(gap), "{one}");
    assert_eq!(cells(&one, "builds"), ["2", "0", "-2"]);
    assert_eq!(cells(&one, "build wall p50 (s)"), ["10.000", "-", "-"]);
    let neither = render_text(&analyze(&no_ids), &analyze(&no_ids));
    assert!(neither.contains(gap), "{neither}");
}

/// Catches: per-build numbers missing from the JSON, a difference of the wrong sign, a gap
/// list that does not follow the data, and per-run values that are not one `RunRecord`
/// (`setting`, `arm`, `run_index`, `seed`, `commit`, `label`, `metrics`) per build under
/// the arm's name.
#[test]
fn the_json_has_per_build_numbers_and_per_run_values() {
    let json: Value =
        serde_json::from_str(&render_json(&analyze(&arm_a()), &analyze(&arm_b()))).unwrap();
    assert_eq!(json["arms"]["a"]["builds"]["count"], 2);
    assert_eq!(json["arms"]["a"]["builds"]["wall_us"]["p50"], 10 * S);
    assert_eq!(json["arms"]["b"]["builds"]["wall_us"]["p90"], 35 * S);
    assert_eq!(json["arms"]["b"]["builds"]["queue_wait_us"]["p90"], 8 * S);
    assert_eq!(json["arms"]["a"]["builds"]["rows_without_id"], 0);
    assert_eq!(json["difference"]["builds"]["wall_us"]["p50"], 5 * S);
    assert_eq!(json["difference"]["builds"]["queue_wait_us"]["p50"], S);
    assert_eq!(json["not_recorded"].as_array().unwrap().len(), 4);
    for (arm, first_wall) in [("a", 30.0e6), ("b", 35.0e6)] {
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
            assert_eq!(r["arm"], arm);
            assert_eq!(r["run_index"], i);
        }
        assert_eq!(runs[0]["metrics"]["build_wall_us"], first_wall);
    }
}

/// Catches: an arm with no ids given numbers (zero) in the JSON where the text says `-`,
/// and a gap list that drops the per-build entry.
#[test]
fn an_arm_with_no_ids_has_null_build_numbers_and_the_full_gap_list() {
    let plain: Vec<Row> = arm_a()
        .into_iter()
        .map(|r| Row {
            invocation_id: None,
            correlated_invocations_id: None,
            ..r
        })
        .collect();
    let stats = analyze(&plain);
    let json: Value = serde_json::from_str(&render_json(&stats, &stats)).unwrap();
    assert_eq!(json["arms"]["a"]["builds"]["count"], 0);
    assert_eq!(json["arms"]["a"]["builds"]["rows_without_id"], 3);
    assert_eq!(json["arms"]["a"]["builds"]["wall_us"]["p50"], Value::Null);
    assert_eq!(json["difference"]["builds"]["wall_us"]["p50"], Value::Null);
    assert_eq!(json["not_recorded"].as_array().unwrap().len(), 5);
    assert_eq!(json["runs"]["a"].as_array().unwrap().len(), 0);
}
