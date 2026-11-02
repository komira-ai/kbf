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

/// Catches: equal labels for the two arms (js-analysis would get the same setting, arm and
/// run index twice), and an empty label (an arm with no name) written into the runs.
#[test]
fn equal_or_empty_labels_are_refused() {
    let dir = with_ids();
    let given = |a: &str, b: &str, setting: &str, commit: &str| {
        verb(
            &dir,
            &[
                "--json",
                "--setting",
                setting,
                "--label-a",
                a,
                "--label-b",
                b,
                "--seed",
                "1",
                "--commit",
                commit,
            ],
        )
    };
    let Err(Failure::Run(text)) = given("js", "js", "k7", "abc") else {
        panic!("equal labels are refused");
    };
    assert!(
        text.contains("--label-a") && text.contains("--label-b"),
        "{text}"
    );
    for (a, b, setting, commit, flag) in [
        ("", "js", "k7", "abc", "--label-a"),
        ("legacy", "", "k7", "abc", "--label-b"),
        ("legacy", "js", "", "abc", "--setting"),
        ("legacy", "js", "k7", "", "--commit"),
    ] {
        let Err(Failure::Run(text)) = given(a, b, setting, commit) else {
            panic!("an empty {flag} is refused");
        };
        assert!(text.contains(flag), "{flag}: {text}");
    }
    assert!(given("legacy", "js", "k7", "abc").is_ok());
}

/// Catches: a label demanded for an arm that has no builds to label, and runs written for
/// it anyway.
#[test]
fn an_arm_with_no_builds_needs_no_label() {
    let dir = dirs(
        &[row("a1", Some("x"), None, (0, 1, 5))],
        &[row("b1", None, None, (0, 2, 9))],
        "compare-builds-cli-one-arm",
    );
    let json: Value = serde_json::from_str(
        &verb(
            &dir,
            &[
                "--json",
                "--setting",
                "k7",
                "--label-a",
                "legacy",
                "--seed",
                "1",
                "--commit",
                "c",
            ],
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(json["runs"]["a"].as_array().unwrap().len(), 1);
    assert_eq!(json["runs"]["b"].as_array().unwrap().len(), 0);
    let Err(Failure::Run(text)) = verb(
        &dir,
        &["--json", "--setting", "k7", "--seed", "1", "--commit", "c"],
    ) else {
        panic!("arm A has builds, so it needs its label");
    };
    assert!(
        text.contains("--label-a") && !text.contains("--label-b"),
        "{text}"
    );
}

/// Catches: the build numbers' limits only in rustdoc: the help of `compare` must say the
/// action span is the server's and excludes the client's own analysis and local actions.
#[test]
fn the_help_says_what_the_build_action_span_leaves_out() {
    let Err(Failure::Usage(help)) = run(
        ["kbf-estimator-replay", "compare", "--help"],
        &mut Vec::new(),
    ) else {
        panic!("help is printed as usage");
    };
    let flat = help.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(flat.contains("build action span (server)"), "{flat}");
    assert!(
        flat.contains("excludes the client's own analysis and local actions"),
        "{flat}"
    );
    assert!(!flat.contains("build wall"), "{flat}");
}

/// Catches: the label of arm A demanded or used when only arm B has builds (the relaxed rule
/// must hold in both directions).
#[test]
fn only_arm_b_having_builds_needs_only_its_label() {
    let dir = dirs(
        &[row("a1", None, None, (0, 1, 5))],
        &[row("b1", Some("x"), None, (0, 2, 9))],
        "compare-builds-cli-b-only",
    );
    let json: Value = serde_json::from_str(
        &verb(
            &dir,
            &[
                "--json",
                "--setting",
                "k7",
                "--label-b",
                "js",
                "--seed",
                "1",
                "--commit",
                "c",
            ],
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(json["runs"]["a"].as_array().unwrap().len(), 0);
    assert_eq!(json["runs"]["b"][0]["arm"], "js");
}
