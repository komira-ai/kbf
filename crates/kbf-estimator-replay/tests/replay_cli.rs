//! The `replay` verb and its report: windows, arms, the limits, and its failures.

mod common;

use common::*;
use kbf_estimator_replay::cli::{Failure, run};
use kbf_estimator_replay::replay::report::{LIMITS, render};
use kbf_estimator_replay::replay::sim::simulate;
use kbf_estimator_replay::replay::{Arm, Params};
use kbf_testkit::dir::TestDir;

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/synthetic");

fn replay(extra: &[&str]) -> Result<String, Failure> {
    let mut args = vec!["kbf-estimator-replay", "replay", "--from", DATA];
    args.extend_from_slice(extra);
    let mut out = Vec::new();
    run(args, &mut out)?;
    Ok(String::from_utf8(out).unwrap())
}

/// Catches a report that leaves out an arm, the fidelity line, the funnel, the limits, or
/// the label that says an arm is not phase 1.
#[test]
fn the_report_names_its_arms_its_funnel_and_its_limits() {
    let text = replay(&["--train-days", "2", "--score-days", "2"]).unwrap();
    for want in [
        "kbf estimator replay (memory only)",
        "parameters (frozen before scoring): ladder [4, 16, 32] GiB, train days 1-2, scored days 3-4",
        "rows: ",
        "rows dropped: none",
        "arm: null (estimator off)",
        "arm: key floor (phase 1)",
        "cold start (day 1)",
        "training window",
        "SCORED window",
        "own-limit kills per 1000 attempts: ",
        "operations that climbed the ladder (killed at least once), per 1000: ",
        "queue wait p50 ",
        "backlog drain after each day with arrivals: ",
        "resolved at L1 ",
        "attempts shown to the estimator: ",
        "ladder re-climbs avoided on scored days, key floor (phase 1): ",
    ] {
        assert!(text.contains(want), "missing {want:?} in\n{text}");
    }
    assert!(
        text.contains("null-arm fidelity: ") && text.contains("(within 1%)"),
        "{text}"
    );
    assert!(!text.contains("upper bound)"), "the third arm is opt-in");
    for limit in LIMITS {
        assert!(text.contains(limit), "missing limit {limit:?}");
    }
    let text = replay(&["--with-upper-bound"]).unwrap();
    assert!(text.contains("arm: sample lowering (NOT phase 1, upper bound)"));
}

/// The part of `text` from the null arm's SCORED window to the next arm.
fn scored(text: &str) -> &str {
    let from = text.find("SCORED window").unwrap();
    let rest = &text[from..];
    &rest[..rest.find("attempts shown").unwrap()]
}

/// Catches the training days scored: parameters are frozen on days 1-N and the headline is
/// days N+1 on. Day 1 has one operation killed at 4 GiB; day 2 has five, which the floor
/// (learned on day 1) books at 16 GiB, and the null arm kills again.
#[test]
fn only_the_days_after_training_are_scored() {
    let mut ops = vec![op("main", 1, 0, 10 * GIB, 100, 100)];
    ops.extend((0..5).map(|i| op("main", 1, DAY_S + 1_000 * i, 10 * GIB, 100, 100)));
    let t = trace(ops, &[("n1", 16, 64)]);
    let params = Params {
        train_days: 1,
        score_days: 1,
        ..Params::default()
    };
    let runs: Vec<_> = [Arm::Null, Arm::KeyFloor]
        .map(|arm| simulate(&t, arm, &params).unwrap())
        .into();
    let text = render(&t, &params, &runs);
    let null = scored(&text);
    assert!(
        null.contains("own-limit kills per 1000 attempts: 5/10 "),
        "{null}"
    );
    assert!(
        null.contains("operations that climbed the ladder (killed at least once), per 1000: 5/5 "),
        "{null}"
    );
    let floor_at = text.find("arm: key floor").unwrap();
    let floor = scored(&text[floor_at..]);
    assert!(
        floor.contains("own-limit kills per 1000 attempts: 0/5 "),
        "{floor}"
    );
    assert!(
        floor.contains("first booking raised by the arm: 5"),
        "{floor}"
    );
    assert!(
        text.contains(
            "ladder re-climbs avoided on scored days, key floor (phase 1): 5 (null 5 kills, arm 0)"
        ),
        "{text}"
    );
    // The training window shows day 1: one kill in two attempts.
    assert!(text.contains("training window: 1 operations"), "{text}");
}

/// Catches an interval that is not an interval: with no events it starts at 0 and has an
/// upper end above 0; with all events it ends at 1000.
#[test]
fn a_rate_has_a_confidence_interval() {
    use kbf_estimator_replay::replay::stats::Rate;
    let (lo, hi) = Rate { hits: 0, of: 100 }.wilson_per_1000();
    assert_eq!(lo, 0.0);
    assert!(hi > 30.0 && hi < 40.0, "{hi}");
    let (lo, hi) = Rate { hits: 100, of: 100 }.wilson_per_1000();
    assert!(lo > 960.0 && lo < 970.0, "{lo}");
    assert!(hi > 999.999, "{hi}");
    let (lo, hi) = Rate { hits: 50, of: 100 }.wilson_per_1000();
    assert!((lo + hi - 1000.0).abs() < 1e-6, "symmetric around half");
    assert_eq!(Rate { hits: 0, of: 0 }.wilson_per_1000(), (0.0, 0.0));
    assert_eq!(Rate { hits: 3, of: 0 }.per_1000(), 0.0);
}

/// Catches a percentile that is not nearest-rank and one that panics on nothing.
#[test]
fn percentiles_are_nearest_rank() {
    use kbf_estimator_replay::replay::stats::percentile;
    let v: Vec<u64> = (1..=100).collect();
    assert_eq!(
        (percentile(&v, 50), percentile(&v, 95), percentile(&v, 99)),
        (Some(50), Some(95), Some(99))
    );
    assert_eq!(percentile(&[7], 99), Some(7));
    assert_eq!(percentile(&[], 50), None);
}

/// Catches each way the verb fails passing as a report.
#[test]
fn the_replay_verb_fails_with_a_reason() {
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "replay-verb");
    let from = tmp.path().display().to_string();
    let mut out = Vec::new();

    let err = run(
        ["r", "replay", "--from", &format!("{from}/missing")],
        &mut out,
    )
    .unwrap_err();
    assert!(
        matches!(err, Failure::Run(ref t) if t.contains("missing")),
        "{err:?}"
    );

    let err = run(["r", "replay", "--from", &from], &mut out).unwrap_err();
    assert!(
        matches!(err, Failure::Run(ref t) if t.contains("no node")),
        "{err:?}"
    );

    let err = run(
        ["r", "replay", "--from", DATA, "--ladder-gib", "8,4"],
        &mut out,
    )
    .unwrap_err();
    assert!(
        matches!(err, Failure::Run(ref t) if t.contains("memory ladder")),
        "{err:?}"
    );

    let err = run(["r", "replay"], &mut out).unwrap_err();
    assert!(
        matches!(err, Failure::Usage(ref t) if t.contains("--from")),
        "{err:?}"
    );

    struct Full;
    impl std::io::Write for Full {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("closed"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let err = run(["r", "replay", "--from", DATA], &mut Full).unwrap_err();
    assert!(
        matches!(err, Failure::Run(ref t) if t.contains("closed")),
        "{err:?}"
    );
}

/// The parameters are the frozen ones unless a flag says otherwise.
#[test]
fn the_default_parameters_are_the_designs() {
    let p = Params::default();
    assert_eq!((p.train_days, p.score_days), (20, 10));
    assert_eq!(p.ladder_gib, [4, 16, 32]);
    assert_eq!(p.first_booking(), 4 * GIB);
    assert_eq!(
        Params {
            ladder_gib: vec![],
            ..p
        }
        .first_booking(),
        4 * GIB
    );
}

/// Catches the dropped-row funnel left out of the report when there are drops, and a
/// comparison line invented when the null arm was not run.
#[test]
fn the_report_lists_dropped_rows_and_compares_only_with_a_null_arm() {
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "replay-drops");
    let rows = vec![
        attempt("a", 0, 5, "ok", Some(GIB)),
        attempt("b", 10, 5, "lost", None),
        attempt("c", 20, 5, "infra", None),
    ];
    kbf_estimator_replay::cli::write_days(&rows, &tmp).unwrap();
    let mut out = Vec::new();
    run(
        ["r", "replay", "--from", &tmp.path().display().to_string()],
        &mut out,
    )
    .unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("rows dropped: infra 1, lost 1"), "{text}");

    let params = Params::default();
    let t = trace(vec![op("main", 1, 0, GIB, 10, 10)], &[("n1", 4, 8)]);
    let only_floor = simulate(&t, Arm::KeyFloor, &params).unwrap();
    let text = render(&t, &params, &[only_floor]);
    assert!(!text.contains("re-climbs avoided"), "{text}");
    assert!(!text.contains("null-arm fidelity"), "{text}");
}
