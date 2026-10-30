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

/// Catches a verb wired to the wrong module, or a stub that reports success: each verb names
/// itself in the refusal.
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
        "http://store.example:9000",
    ];
    assert_eq!(not_yet(run(&proxy).await), "fault-proxy");
    let latency = [proxy.as_slice(), &["--mode", "latency=50"]].concat();
    assert_eq!(not_yet(run(&latency).await), "fault-proxy");
}

/// Catches a cell flag that runs another cell, and the request log not being made where a run's
/// results go.
#[tokio::test]
async fn each_cell_flag_runs_its_own_cell_and_the_run_makes_its_request_log() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-cells");
    let out = dir.path().join("out");
    for (cell, name) in [
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

/// Catches cold-put starting without a run id (which names every key it writes) or without a
/// cold store to write to: each is refused before a request, with its own error.
#[tokio::test]
async fn cold_put_needs_a_run_id_and_a_cold_store() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-cp-args");
    let out = dir.path().join("out");
    let out = out.to_str().unwrap();
    let no_id = ["run", "--result-dir", out, "--cell", "cold-put"];
    assert!(matches!(run(&no_id).await, Err(BenchError::Request(_))));
    let no_store = [
        "run",
        "--result-dir",
        out,
        "--cell",
        "cold-put",
        "--run-id",
        "r1",
    ];
    assert!(matches!(run(&no_store).await, Err(BenchError::Config(_))));
}

fn cold_run_file(dir: &TestDir) -> std::path::PathBuf {
    let path = dir.path().join("run.json");
    std::fs::write(
        &path,
        r#"{"stores": {"cold": {"endpoint": "http://127.0.0.1:9", "bucket": "b",
            "access_key_id": "k", "secret_access_key": "s"}}}"#,
    )
    .unwrap();
    path
}

/// Catches a halted run reported as a success (the farm script would go on to the next level),
/// and a halted run that leaves no result: the stop file makes `run` an error that names it,
/// after the result of the level that did not start is written.
#[tokio::test]
async fn a_stop_file_makes_the_run_verb_fail_with_the_reason_and_leaves_its_result() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-cp-halt");
    let out = dir.path().join("out");
    let stop = dir.path().join("stop");
    std::fs::write(&stop, "").unwrap();
    let run_file = cold_run_file(&dir);
    let args = [
        "run",
        "--result-dir",
        out.to_str().unwrap(),
        "--run-file",
        run_file.to_str().unwrap(),
        "--cell",
        "cold-put",
        "--run-id",
        "r1",
        "--stop-file",
        stop.to_str().unwrap(),
    ];
    match run(&args).await {
        Err(BenchError::Halted(why)) => assert!(why.contains("stop-file"), "{why}"),
        other => panic!("expected a halt, got {other:?}"),
    }
    assert!(out.join("cold-put-direct-c1.json").is_file());
}

/// Catches a cleanup that makes up an empty log when there is none (and so reports a clean
/// success), and one that does not need to be told which run.
#[tokio::test]
async fn cleanup_without_a_log_is_an_error_and_makes_none() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-cl-args");
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let run_file = cold_run_file(&dir);
    let args = [
        "cleanup",
        "--result-dir",
        out.to_str().unwrap(),
        "--run-file",
        run_file.to_str().unwrap(),
        "--run-id",
        "r1",
    ];
    assert!(matches!(run(&args).await, Err(BenchError::Io { .. })));
    assert!(!out.join("requests.jsonl").exists());
}

/// Catches usage the parser must refuse: no cell, a one-pass run (no pass left after the
/// warm-up), an unknown cell, a mode that is no fault, and a cleanup that names no run.
#[test]
fn bad_usage_is_refused_by_the_parser() {
    let bad: [&[&str]; 5] = [
        &["cleanup", "--result-dir", "o"],
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
