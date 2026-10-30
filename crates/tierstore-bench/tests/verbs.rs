//! The six verbs, in process: each reaches its module, and the stubs say which one is not built.

use clap::Parser;
use kbf_testkit::dir::TestDir;
use tierstore_bench::cli::{self, Cli};
use tierstore_bench::error::BenchError;

async fn run(args: &[&str]) -> Result<(), BenchError> {
    let mut argv = vec!["tierstore-bench"];
    argv.extend_from_slice(args);
    cli::run(Cli::try_parse_from(argv).unwrap()).await
}

fn not_yet(result: Result<(), BenchError>) -> &'static str {
    match result {
        Err(BenchError::NotYet(name)) => name,
        other => panic!("expected a stub, got {other:?}"),
    }
}

/// Catches a verb wired to the wrong module, or a stub that reports success: each stub verb
/// names itself in the refusal, and the fault proxy refuses an upstream it cannot dial.
#[tokio::test]
async fn every_verb_reaches_its_module_and_a_stub_says_so() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-verbs");
    let out = dir.path().join("out");
    let out = out.to_str().unwrap();
    let snapshot = dir.path().join("before.json");
    let snapshot = snapshot.to_str().unwrap();

    assert_eq!(
        not_yet(run(&["preflight", "--result-dir", out]).await),
        "preflight"
    );
    assert_eq!(
        not_yet(run(&["provision", "--result-dir", out]).await),
        "provision"
    );
    assert_eq!(
        not_yet(run(&["provision", "--result-dir", out, "--apply"]).await),
        "provision"
    );
    assert_eq!(
        not_yet(run(&["verify-teardown", "--result-dir", out, "--before", snapshot]).await),
        "verify-teardown"
    );
    assert_eq!(
        not_yet(run(&["report", "--result-dir", out]).await),
        "report"
    );
    let proxy = [
        "fault-proxy",
        "--listen",
        "127.0.0.1:0",
        "--upstream",
        "store.example:9000",
    ];
    assert!(matches!(run(&proxy).await, Err(BenchError::Proxy(_))));
}

/// Catches a cell flag that runs another cell, and the request log not being made where a run's
/// results go.
#[tokio::test]
async fn each_cell_flag_runs_its_own_cell_and_the_run_makes_its_request_log() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-cells");
    let out = dir.path().join("out");
    for (cell, name) in [
        ("cold-put", "cell cold-put"),
        ("cold-read", "cell cold-read"),
        ("a1", "cell a1"),
        ("b", "cell b"),
        ("c", "cell c"),
        ("g", "cell g"),
    ] {
        let args = [
            "run",
            "--result-dir",
            out.to_str().unwrap(),
            "--cell",
            cell,
            "--passes",
            "3",
        ];
        assert_eq!(not_yet(run(&args).await), name);
    }
    assert!(out.join("requests.jsonl").is_file());
}

/// Catches a result directory that cannot be made being ignored.
#[tokio::test]
async fn a_result_directory_that_cannot_be_made_stops_the_run() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-nodir");
    let file = dir.path().join("a-file");
    std::fs::write(&file, "x").unwrap();
    let under_file = file.join("out");
    let args = [
        "run",
        "--result-dir",
        under_file.to_str().unwrap(),
        "--cell",
        "a1",
    ];
    assert!(matches!(run(&args).await, Err(BenchError::Io { .. })));
}

/// Catches usage the parser must refuse: no cell, a one-pass run (no pass left after the
/// warm-up), an unknown cell and a mode that is no fault.
#[test]
fn bad_usage_is_refused_by_the_parser() {
    let bad: [&[&str]; 4] = [
        &["run", "--result-dir", "o"],
        &["run", "--result-dir", "o", "--cell", "a1", "--passes", "1"],
        &["run", "--result-dir", "o", "--cell", "z9"],
        &[
            "fault-proxy",
            "--listen",
            "127.0.0.1:0",
            "--upstream",
            "u",
            "--mode",
            "slow",
        ],
    ];
    for args in bad {
        let mut argv = vec!["tierstore-bench"];
        argv.extend_from_slice(args);
        assert!(Cli::try_parse_from(argv).is_err(), "{args:?}");
    }
}
