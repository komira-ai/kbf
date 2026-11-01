//! The slowdown report: which runs are quiet, the baseline, the arithmetic, the summaries,
//! the order of the output, and the `slowdown` verb. Nothing here measures time: every wall
//! is a number written into a row.

mod common;

use common::*;
use kbf_estimator_replay::cli::{Failure, run, write_days};
use kbf_estimator_replay::row::Row;
use kbf_estimator_replay::slowdown::{Params, Quiet, Summary, analyze, render_json, render_text};
use kbf_testkit::dir::TestDir;

const S: u64 = 1_000_000;
/// Quiet: below the default 5.00% threshold. Busy: above it.
const CALM: u64 = 100;
const BUSY: u64 = 3_000;

/// A successful run of shape `aa` on `node` from `start_s` for `wall_s` seconds, with the
/// node's CPU pressure at its start.
fn ok(id: &str, node: &str, start_s: u64, wall_s: u64, pressure: Option<u64>) -> Row {
    let mut r = attempt(id, 0, wall_s, "ok", None);
    r.run_id = id.to_owned();
    r.node = node.to_owned();
    r.queued_us = OCT_1 + start_s * S;
    r.started_us = Some(OCT_1 + start_s * S);
    r.ended_us = Some(OCT_1 + (start_s + wall_s) * S);
    r.node_cpu_pressure_centi = pressure;
    r
}

fn shaped(mut r: Row, shape: &str) -> Row {
    r.shape = shape.to_owned();
    r
}

fn walls(report: &kbf_estimator_replay::slowdown::Report) -> Vec<(String, Option<f64>)> {
    report
        .runs
        .iter()
        .map(|r| (r.run_id.clone(), r.slowdown))
        .collect()
}

/// Catches a baseline taken over every run: the busy run is the fastest (50 s) and the
/// quiet one took 100 s, so a baseline that let the busy runs in would be 50.
#[test]
fn the_baseline_is_taken_from_quiet_runs_only() {
    let rows = vec![
        ok("q", "n1", 100, 100, Some(CALM)),
        ok("b1", "n1", 1_000, 50, Some(BUSY)),
        ok("b2", "n1", 2_000, 250, Some(BUSY)),
    ];
    let report = analyze(&rows, &Params::default());
    assert_eq!(report.keys.len(), 1);
    assert_eq!(report.keys[0].baseline_us, Some(100 * S));
    assert_eq!(report.keys[0].quiet_runs, 1);
    assert_eq!(
        walls(&report),
        vec![
            ("q".into(), Some(1.0)),
            ("b1".into(), Some(0.5)),
            ("b2".into(), Some(2.5)),
        ]
    );
    assert_eq!(report.runs[0].quiet, Quiet::Quiet);
    assert_eq!(report.runs[1].quiet, Quiet::Busy);
}

/// Catches a quiet rule that ignores what else ran on the node: a run with a calm reading
/// that overlapped another exported attempt on its node is busy; the same time on another
/// node, are not overlap; `max_overlap` raises the allowance.
#[test]
fn a_run_that_overlapped_another_on_its_node_is_not_quiet() {
    let rows = vec![
        // Overlaps `other` on n1 (100..200 against 150..250).
        ok("a", "n1", 100, 100, Some(CALM)),
        ok("other", "n1", 150, 100, Some(BUSY)),
        // Same time, other node: quiet. Touching attempts (ends where the next starts) too.
        ok("c", "n2", 150, 100, Some(CALM)),
        ok("d", "n3", 100, 100, Some(CALM)),
        ok("e", "n3", 200, 100, Some(CALM)),
    ];
    let report = analyze(&rows, &Params::default());
    let state = |id: &str| report.runs.iter().find(|r| r.run_id == id).unwrap().quiet;
    assert_eq!(state("a"), Quiet::Busy);
    assert_eq!(state("other"), Quiet::Busy);
    assert_eq!(state("c"), Quiet::Quiet);
    assert_eq!(state("d"), Quiet::Quiet);
    assert_eq!(state("e"), Quiet::Quiet);
    let lenient = analyze(
        &rows,
        &Params {
            max_overlap: 1,
            ..Params::default()
        },
    );
    assert_eq!(
        lenient.runs.iter().find(|r| r.run_id == "a").unwrap().quiet,
        Quiet::Quiet
    );
}

/// Catches overlap that counts a run against itself, or that counts attempts that are not
/// ok (a failed attempt on the node still took the CPU).
#[test]
fn every_exported_attempt_on_the_node_counts_as_load_whatever_its_outcome() {
    let mut failed = ok("f", "n1", 150, 100, None);
    failed.outcome = "failed".into();
    let rows = vec![ok("a", "n1", 100, 100, Some(CALM)), failed];
    let report = analyze(&rows, &Params::default());
    assert_eq!(report.runs.len(), 1);
    assert_eq!(report.runs[0].quiet, Quiet::Busy);
    let alone = analyze(&rows[..1], &Params::default());
    assert_eq!(alone.runs[0].quiet, Quiet::Quiet);
}

/// Catches an imputed baseline: a key whose runs are all busy, or have no pressure reading,
/// has no baseline; its runs carry no slowdown, are counted, and stay out of the summary.
#[test]
fn a_key_with_no_quiet_run_is_counted_not_imputed() {
    let rows = vec![
        ok("q", "n1", 100, 100, Some(CALM)),
        shaped(ok("x1", "n1", 1_000, 80, Some(BUSY)), "00000000000000bb"),
        shaped(ok("x2", "n1", 2_000, 90, None), "00000000000000bb"),
        shaped(ok("y1", "n1", 3_000, 90, None), "00000000000000cc"),
    ];
    let report = analyze(&rows, &Params::default());
    assert_eq!(report.keys_without_baseline, 2);
    assert_eq!(report.runs_without_baseline, 3);
    assert_eq!(report.unknown_pressure, 2);
    assert_eq!(report.overall.runs, 1);
    let x1 = report.runs.iter().find(|r| r.run_id == "x1").unwrap();
    assert_eq!((x1.baseline_us, x1.slowdown), (None, None));
    assert_eq!(
        report.runs.iter().find(|r| r.run_id == "x2").unwrap().quiet,
        Quiet::Unknown
    );
    let with_baseline: Vec<_> = report
        .keys
        .iter()
        .filter(|k| k.baseline_us.is_some())
        .collect();
    assert_eq!(with_baseline.len(), 1);
    let text = render_text(&report);
    assert!(
        text.contains("keys: 1 with a quiet baseline, 2 without"),
        "{text}"
    );
    assert!(
        text.contains("3 runs without a baseline (not imputed)"),
        "{text}"
    );
}

/// Catches the minimum used as the baseline (one fast outlier among twenty quiet runs sets
/// it) and a rank off by one: the baseline is the 10th percentile by nearest rank, here the
/// second smallest of twenty.
#[test]
fn the_baseline_is_the_tenth_percentile_of_the_quiet_walls() {
    let mut rows = vec![ok("outlier", "n1", 100, 10, Some(CALM))];
    rows.extend((0..19).map(|i| ok(&format!("r{i:02}"), "n1", 1_000 * (i + 2), 100, Some(CALM))));
    let report = analyze(&rows, &Params::default());
    assert_eq!(report.keys[0].baseline_us, Some(100 * S));
    assert_eq!(report.keys[0].quiet_runs, 20);
    let p50 = analyze(
        &rows,
        &Params {
            baseline_percentile: 50,
            ..Params::default()
        },
    );
    assert_eq!(p50.keys[0].baseline_us, Some(100 * S));
    let p1 = analyze(
        &rows,
        &Params {
            baseline_percentile: 1,
            ..Params::default()
        },
    );
    assert_eq!(p1.keys[0].baseline_us, Some(10 * S));
}

/// Catches slowdown arithmetic that rounds, truncates or divides the wrong way round:
/// baseline 100 s, runs of 100, 150, 151 and 400 s.
#[test]
fn slowdown_is_wall_over_baseline() {
    let rows = vec![
        ok("q", "n1", 100, 100, Some(CALM)),
        ok("w150", "n1", 1_000, 150, Some(BUSY)),
        ok("w151", "n1", 2_000, 151, Some(BUSY)),
        ok("w400", "n1", 3_000, 400, Some(BUSY)),
    ];
    let report = analyze(&rows, &Params::default());
    assert_eq!(
        walls(&report),
        vec![
            ("q".into(), Some(1.0)),
            ("w150".into(), Some(1.5)),
            ("w151".into(), Some(1.51)),
            ("w400".into(), Some(4.0)),
        ]
    );
    // Strictly above 1.5x: 150 s is on the line, not over it.
    assert_eq!(report.overall.above_1_5, 2);
}

/// Catches a median or p90 that is not nearest-rank, and a share that includes the line:
/// ten runs against a 100 s baseline.
#[test]
fn summaries_of_a_known_set() {
    let pairs: Vec<(u64, u64)> = [100, 100, 100, 100, 100, 150, 160, 200, 300, 500]
        .into_iter()
        .map(|w| (w * S, 100 * S))
        .collect();
    let s = Summary::of(&pairs);
    assert_eq!(s.runs, 10);
    assert_eq!(s.median, Some(1.0));
    assert_eq!(s.p90, Some(3.0));
    assert_eq!(s.above_1_5, 4);
    let one = Summary::of(&pairs[9..]);
    assert_eq!(
        (one.runs, one.median, one.p90, one.above_1_5),
        (1, Some(5.0), Some(5.0), 1)
    );
    let none = Summary::of(&[]);
    assert_eq!(
        (none.runs, none.median, none.p90, none.above_1_5),
        (0, None, None, 0)
    );
}

/// Catches rows that are not measurements being measured: a run that did not succeed, one
/// with no start or end (or none apart), one with no shape. Each is counted by reason and
/// none enters a baseline.
#[test]
fn what_is_not_a_measurement_is_counted_by_reason() {
    let mut failed = ok("failed", "n1", 100, 10, Some(CALM));
    failed.outcome = "failed".into();
    let mut unstarted = ok("unstarted", "n1", 200, 10, Some(CALM));
    unstarted.started_us = None;
    let mut unended = ok("unended", "n1", 300, 10, Some(CALM));
    unended.ended_us = None;
    let mut instant = ok("instant", "n1", 400, 10, Some(CALM));
    instant.ended_us = instant.started_us;
    let shapeless = shaped(ok("shapeless", "n1", 500, 10, Some(CALM)), "-");
    let rows = vec![
        failed,
        unstarted,
        unended,
        instant,
        shapeless,
        ok("good", "n1", 600, 100, Some(CALM)),
    ];
    let report = analyze(&rows, &Params::default());
    assert_eq!(report.funnel.rows, 6);
    assert_eq!(report.funnel.not_ok, 1);
    assert_eq!(report.funnel.no_wall, 3);
    assert_eq!(report.funnel.no_shape, 1);
    assert_eq!(report.funnel.measured, 1);
    assert_eq!(report.keys[0].baseline_us, Some(100 * S));
}

/// Catches a key that merges what the estimator keeps apart: another instance, platform
/// class, basis, shape or cfg each has its own baseline (the same wall is 1.0 against its
/// own, 4.0 against a 25 s neighbour).
#[test]
fn runs_are_grouped_by_the_estimators_key() {
    let mut other = vec![];
    for (i, edit) in [
        |r: &mut Row| r.instance = "aux".into(),
        |r: &mut Row| r.platform = "macos".into(),
        |r: &mut Row| r.basis = Some("mac_footprint".into()),
        |r: &mut Row| r.shape = "00000000000000bb".into(),
        |r: &mut Row| r.cfg = "release".into(),
    ]
    .into_iter()
    .enumerate()
    {
        let mut r = ok(
            &format!("o{i}"),
            "n1",
            1_000 * (i as u64 + 2),
            100,
            Some(CALM),
        );
        edit(&mut r);
        other.push(r);
    }
    let mut rows = vec![ok("base", "n1", 100, 25, Some(CALM))];
    rows.extend(other);
    let report = analyze(&rows, &Params::default());
    assert_eq!(report.keys.len(), 6);
    assert!(report.keys.iter().all(|k| k.baseline_us.is_some()));
    assert!(
        report.runs.iter().all(|r| r.slowdown == Some(1.0)),
        "{:?}",
        walls(&report)
    );
}

/// Catches output whose order follows the input: the same rows in three orders give the same
/// report text and JSON, runs by start then id, keys by key.
#[test]
fn output_does_not_depend_on_input_order() {
    let rows = vec![
        ok("q", "n1", 100, 100, Some(CALM)),
        ok("b", "n1", 1_000, 300, Some(BUSY)),
        shaped(ok("z", "n2", 100, 40, Some(CALM)), "00000000000000bb"),
        shaped(ok("y", "n2", 1_000, 60, Some(BUSY)), "00000000000000bb"),
        ok("a", "n3", 1_000, 120, Some(BUSY)),
    ];
    let mut reversed = rows.clone();
    reversed.reverse();
    let mut rotated = rows.clone();
    rotated.rotate_left(2);
    let first = analyze(&rows, &Params::default());
    for other in [&reversed, &rotated] {
        let again = analyze(other, &Params::default());
        assert_eq!(render_text(&first), render_text(&again));
        assert_eq!(render_json(&first), render_json(&again));
    }
    let order: Vec<_> = first.runs.iter().map(|r| r.run_id.as_str()).collect();
    // Start time, then run id: q and z start at 100, then a, b and y at 1000.
    assert_eq!(order, ["q", "z", "a", "b", "y"]);
    let keys: Vec<_> = first.keys.iter().map(|k| k.key.as_str()).collect();
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    assert_eq!(keys, sorted);
}

fn dir_with(rows: &[Row], name: &str) -> TestDir {
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    write_days(rows, &tmp).unwrap();
    tmp
}

fn verb(dir: &TestDir, extra: &[&str]) -> Result<String, Failure> {
    let mut args = vec![
        "kbf-estimator-replay".to_owned(),
        "slowdown".into(),
        "--from".into(),
        dir.to_string_lossy().into_owned(),
    ];
    args.extend(extra.iter().map(|s| (*s).to_owned()));
    let mut out = Vec::new();
    run(args, &mut out)?;
    Ok(String::from_utf8(out).unwrap())
}

/// Catches the verb not reading the export, ignoring its flags, or printing no limits; and
/// the JSON form not being JSON with the runs in it.
#[test]
fn the_verb_reads_an_export_and_prints_text_or_json() {
    let rows = vec![
        ok("q", "n1", 100, 100, Some(CALM)),
        ok("b", "n1", 1_000, 300, Some(BUSY)),
        shaped(ok("n", "n1", 2_000, 70, None), "00000000000000bb"),
    ];
    let dir = dir_with(&rows, "slowdown-verb");
    let text = verb(&dir, &[]).unwrap();
    for want in [
        "slowdown on busy hosts",
        "node CPU pressure at lease start <= 5.00%",
        "baseline: percentile 10 (nearest rank)",
        "keys: 1 with a quiet baseline, 1 without",
        "overall (2 runs): median 1.00x, p90 3.00x, above 1.5x: 1/2",
    ] {
        assert!(text.contains(want), "missing {want:?} in\n{text}");
    }
    for limit in kbf_estimator_replay::slowdown::LIMITS {
        assert!(text.contains(limit), "missing limit {limit:?}");
    }
    // Flags move the result: a threshold above the busy reading makes that run quiet, and the
    // 100th percentile of the quiet walls (300 s) is the baseline, so nothing is slow.
    let lenient = verb(
        &dir,
        &[
            "--quiet-pressure-centi",
            "5000",
            "--baseline-percentile",
            "100",
        ],
    )
    .unwrap();
    assert!(lenient.contains("above 1.5x: 0/2"), "{lenient}");
    let json: serde_json::Value = serde_json::from_str(&verb(&dir, &["--json"]).unwrap()).unwrap();
    assert_eq!(json["runs"].as_array().unwrap().len(), 3);
    assert_eq!(json["overall"]["runs"], 2);
    assert_eq!(json["keys_without_baseline"], 1);
    assert_eq!(json["params"]["baseline_percentile"], 10);
    assert_eq!(json["runs"][0]["run_id"], "q");
    assert_eq!(json["runs"][1]["slowdown"], 3.0);
    assert_eq!(json["runs"][2]["slowdown"], serde_json::Value::Null);
}

/// Catches a percentile outside 1..=100 accepted and a missing export reported as success.
#[test]
fn bad_arguments_and_a_missing_export_fail() {
    let dir = dir_with(&[ok("q", "n1", 100, 100, Some(CALM))], "slowdown-bad");
    for bad in ["0", "101"] {
        let err = verb(&dir, &["--baseline-percentile", bad]).unwrap_err();
        assert!(matches!(err, Failure::Usage(_)), "{err:?}");
    }
    let missing = dir.join("none");
    let err = run(
        [
            "kbf-estimator-replay",
            "slowdown",
            "--from",
            &missing.to_string_lossy(),
        ],
        &mut Vec::new(),
    )
    .unwrap_err();
    assert!(
        matches!(err, Failure::Run(ref t) if t.contains("none")),
        "{err:?}"
    );
}

/// Catches a report with no quiet run anywhere that divides by nothing, prints a number for
/// an absent one, or hides the keys it could not score.
#[test]
fn a_history_with_no_quiet_run_says_so() {
    let rows = vec![
        ok("b", "n1", 100, 100, Some(BUSY)),
        ok("u", "n1", 1_000, 100, None),
    ];
    let report = analyze(&rows, &Params::default());
    assert_eq!(report.overall, Summary::default());
    let text = render_text(&report);
    assert!(
        text.contains("overall (0 runs): median -, p90 -, above 1.5x: 0/0 = -"),
        "{text}"
    );
    assert!(text.contains("baseline none"), "{text}");
    assert!(
        text.contains("keys: 0 with a quiet baseline, 1 without"),
        "{text}"
    );
    assert!(
        text.contains("measured runs with no pressure reading: 1"),
        "{text}"
    );
}

/// Catches nearest rank done with floor division. Median of 3 ratios 1, 2, 3: rank
/// ceil(50*3/100) = 2, so 2.0 (floor gives rank 1). p90 of 3: rank ceil(2.7) = 3, so 3.0.
/// p90 of 11 ratios 1..=11: rank ceil(9.9) = 10, so 10.0 (floor gives 9).
#[test]
fn nearest_rank_rounds_up_when_the_rank_is_not_whole() {
    let three: Vec<(u64, u64)> = [1, 2, 3].map(|w| (w * S, S)).into();
    let s = Summary::of(&three);
    assert_eq!((s.median, s.p90), (Some(2.0), Some(3.0)));
    let eleven: Vec<(u64, u64)> = (1..=11).map(|w| (w * S, S)).collect();
    let s = Summary::of(&eleven);
    // Median of 11: rank ceil(5.5) = 6.
    assert_eq!((s.median, s.p90), (Some(6.0), Some(10.0)));
}

/// Catches the baseline rank rounded down: 11 quiet walls of 10, 20, ..., 110 s; p10 is rank
/// ceil(10*11/100) = 2, so 20 s (floor gives the minimum, 10 s).
#[test]
fn the_baseline_rank_rounds_up_when_not_whole() {
    let rows: Vec<Row> = (1..=11)
        .map(|i| ok(&format!("r{i:02}"), "n1", 1_000 * i, 10 * i, Some(CALM)))
        .collect();
    let report = analyze(&rows, &Params::default());
    assert_eq!(report.keys[0].quiet_runs, 11);
    assert_eq!(report.keys[0].baseline_us, Some(20 * S));
}

/// Catches `<=` turned into `<` on the pressure threshold: exactly 500 is quiet, 501 busy.
#[test]
fn the_pressure_threshold_is_inclusive() {
    let rows = vec![
        ok("at", "n1", 100, 100, Some(500)),
        ok("over", "n1", 1_000, 100, Some(501)),
    ];
    let report = analyze(&rows, &Params::default());
    let state = |id: &str| report.runs.iter().find(|r| r.run_id == id).unwrap().quiet;
    assert_eq!(state("at"), Quiet::Quiet);
    assert_eq!(state("over"), Quiet::Busy);
}
