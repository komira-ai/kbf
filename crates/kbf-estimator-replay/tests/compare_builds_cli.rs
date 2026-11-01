//! The `compare` verb's flags for per-build numbers: `--group-by`, and the labels of the
//! per-run JSON (`--setting`, `--label-a`, `--label-b`, `--seed`, `--commit`), which the
//! verb requires before it writes runs and refuses without `--json`.

mod common;

use std::path::Path;

use common::*;
use kbf_estimator_replay::cli::{Failure, run, write_days};
use kbf_estimator_replay::row::Row;
use kbf_testkit::dir::TestDir;
use serde_json::Value;

const S: u64 = 1_000_000;

fn row(id: &str, inv: Option<&str>, corr: Option<&str>, times: (u64, u64, u64)) -> Row {
    Row {
        run_id: id.into(),
        queued_us: OCT_1 + times.0 * S,
        started_us: Some(OCT_1 + times.1 * S),
        ended_us: Some(OCT_1 + times.2 * S),
        invocation_id: inv.map(str::to_owned),
        correlated_invocations_id: corr.map(str::to_owned),
        ..attempt(id, 0, 1, "ok", None)
    }
}

fn dirs(a: &[Row], b: &[Row], name: &str) -> TestDir {
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    write_days(a, &tmp.join("a")).unwrap();
    write_days(b, &tmp.join("b")).unwrap();
    tmp
}

fn verb(dir: &Path, extra: &[&str]) -> Result<String, Failure> {
    let mut args = vec![
        "kbf-estimator-replay".to_owned(),
        "compare".into(),
        "--arm-a".into(),
        dir.join("a").to_string_lossy().into_owned(),
        "--arm-b".into(),
        dir.join("b").to_string_lossy().into_owned(),
    ];
    args.extend(extra.iter().map(|s| (*s).to_owned()));
    let mut out = Vec::new();
    run(args, &mut out)?;
    Ok(String::from_utf8(out).unwrap())
}

const LABELS: [&str; 10] = [
    "--setting",
    "k7",
    "--label-a",
    "legacy",
    "--label-b",
    "js",
    "--seed",
    "9",
    "--commit",
    "deadbeef",
];

fn with_ids() -> TestDir {
    dirs(
        &[row("a1", Some("x"), Some("job"), (0, 1, 5))],
        &[row("b1", Some("x"), Some("job"), (0, 2, 9))],
        "compare-builds-cli",
    )
}

/// Catches: runs written without the labels the export cannot know (a made-up setting,
/// seed or commit would be taken for a measurement), and an error that does not name what
/// is missing.
#[test]
fn json_runs_are_refused_without_the_labels_and_the_error_names_them() {
    let dir = with_ids();
    let Err(Failure::Run(text)) = verb(&dir, &["--json"]) else {
        panic!("refused");
    };
    for flag in ["--setting", "--label-a", "--label-b", "--seed", "--commit"] {
        assert!(text.contains(flag), "{flag}: {text}");
    }
    let Err(Failure::Run(text)) = verb(&dir, &["--json", "--setting", "k7", "--seed", "1"]) else {
        panic!("refused");
    };
    assert!(
        text.contains("--label-a") && text.contains("--commit"),
        "{text}"
    );
    assert!(!text.contains("--setting"), "{text}");
}

/// Catches: labels that do not reach the runs (arm names, setting, seed and commit from the
/// flags), arm A's label on arm B's runs, and the group-by default.
#[test]
fn json_runs_carry_the_flag_labels() {
    let dir = with_ids();
    let mut args = vec!["--json"];
    args.extend(LABELS);
    let json: Value = serde_json::from_str(&verb(&dir, &args).unwrap()).unwrap();
    let a = &json["runs"]["a"][0];
    let b = &json["runs"]["b"][0];
    assert_eq!(
        (&a["arm"], &b["arm"]),
        (&Value::from("legacy"), &Value::from("js"))
    );
    for r in [a, b] {
        assert_eq!(r["setting"], "k7");
        assert_eq!(r["seed"], 9);
        assert_eq!(r["commit"], "deadbeef");
    }
    assert_eq!(a["metrics"]["build_action_span_us"], 5.0 * 1e6);
    assert_eq!(b["metrics"]["build_action_span_us"], 9.0 * 1e6);
}

/// Catches: labels accepted where they would do nothing (a table has no runs), and a
/// verb that refuses `--json` for arms with no builds (the older use must still work).
#[test]
fn labels_are_refused_without_json_and_not_needed_without_builds() {
    let dir = with_ids();
    assert!(matches!(verb(&dir, &LABELS), Err(Failure::Run(_))));
    let plain = dirs(
        &[row("a1", None, None, (0, 1, 5))],
        &[row("b1", None, None, (0, 2, 9))],
        "compare-builds-cli-plain",
    );
    let json: Value = serde_json::from_str(&verb(&plain, &["--json"]).unwrap()).unwrap();
    assert_eq!(json["arms"]["a"]["builds"]["count"], 0);
    assert!(json.get("runs").is_none());
}

/// Catches: `--group-by` ignored (the CI-job view would show the invocation view), a value
/// that is not one of the two accepted, and labels accepted with no builds to label.
#[test]
fn group_by_correlated_gives_the_ci_job_view() {
    let a = [
        row("a1", Some("x"), Some("job"), (0, 1, 5)),
        row("a2", Some("y"), Some("job"), (1, 2, 7)),
    ];
    let b = [row("b1", Some("x"), None, (0, 2, 9))];
    let dir = dirs(&a, &b, "compare-builds-cli-group");
    let invocation = verb(&dir, &[]).unwrap();
    let job = verb(&dir, &["--group-by", "correlated"]).unwrap();
    let cell = |text: &str, label: &str| {
        text.lines()
            .find(|l| l.len() > 34 && l[..34].trim() == label)
            .map(|l| {
                l[34..]
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap()
    };
    assert_eq!(cell(&invocation, "builds"), ["2", "1", "-1"]);
    assert_eq!(cell(&job, "builds"), ["1", "0", "-1"]);
    assert_eq!(cell(&job, "build rows with no id"), ["0", "1", "1"]);
    let explicit = verb(&dir, &["--group-by", "invocation"]).unwrap();
    assert_eq!(explicit, invocation);
    let bad = verb(&dir, &["--group-by", "owner"]);
    assert!(matches!(bad, Err(Failure::Usage(_))), "{bad:?}");
}
