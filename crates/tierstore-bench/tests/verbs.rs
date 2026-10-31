//! The six verbs, in process: each reaches its module; the ops verbs begin with the ops file of the
//! result directory, and the fault proxy refuses an upstream it cannot dial.

use clap::Parser;
use kbf_testkit::dir::TestDir;
use tierstore_bench::cli::{self, Cli};
use tierstore_bench::error::BenchError;
use tierstore_bench::preflight::{self, PreflightReport};

async fn run(args: &[&str]) -> Result<(), BenchError> {
    let mut argv = vec!["tierstore-bench"];
    argv.extend_from_slice(args);
    cli::run(Cli::try_parse_from(argv).unwrap()).await
}

/// Catches a verb wired to the wrong module: each ops verb reads its ops file first, and the fault
/// proxy refuses an upstream it cannot dial.
#[tokio::test]
async fn every_verb_reaches_its_module() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-verbs");
    let out = dir.path().join("out");
    let out = out.to_str().unwrap();
    let snapshot = dir.path().join("before.json");
    let snapshot = snapshot.to_str().unwrap();

    // The ops verbs read `ops.json` of the result directory first; there is none here.
    let ops_verbs: [&[&str]; 4] = [
        &["preflight", "--result-dir", out, "--test-prefix", "tb-"],
        &["provision", "--result-dir", out, "--test-prefix", "tb-"],
        &[
            "provision",
            "--result-dir",
            out,
            "--test-prefix",
            "tb-",
            "--apply",
        ],
        &[
            "verify-teardown",
            "--result-dir",
            out,
            "--test-prefix",
            "tb-",
            "--before",
            snapshot,
        ],
    ];
    for args in ops_verbs {
        match run(args).await {
            Err(BenchError::Io { path, .. }) => assert!(path.ends_with("ops.json"), "{args:?}"),
            other => panic!("{args:?}: {other:?}"),
        }
    }
    let proxy = [
        "fault-proxy",
        "--listen",
        "127.0.0.1:0",
        "--upstream",
        "store.example:9000",
    ];
    assert!(matches!(run(&proxy).await, Err(BenchError::Proxy(_))));
}

/// A `run` command line for `out`, with the flags every run needs.
fn run_args(out: &std::path::Path, extra: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = [
        "run",
        "--run-id",
        "r1",
        "--git-sha",
        "abc",
        "--cell",
        "a1",
        "--passes",
        "3",
        "--test-prefix",
        "tb-",
        "--result-dir",
    ]
    .map(String::from)
    .to_vec();
    args.push(out.to_str().unwrap().to_owned());
    // The campaign ledger lies beside the result directory; the first run of a test begins it.
    let ledger = ledger_of(out);
    args.push("--campaign-ledger".to_owned());
    args.push(ledger.to_str().unwrap().to_owned());
    if !ledger.exists() {
        args.push("--new-campaign".to_owned());
    }
    args.extend(extra.iter().map(|s| (*s).to_owned()));
    args
}

/// The ledger file beside the result directory `out`.
fn ledger_of(out: &std::path::Path) -> std::path::PathBuf {
    out.with_file_name("campaign-ledger.json")
}

async fn run_owned(args: &[String]) -> Result<(), BenchError> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run(&args).await
}

fn write_report(out: &std::path::Path, report: &PreflightReport) {
    std::fs::create_dir_all(out).unwrap();
    std::fs::write(
        out.join(preflight::REPORT_FILE),
        serde_json::to_vec(report).unwrap(),
    )
    .unwrap();
}

/// Catches the request log not being made where a run's results go, and a run of a cell with no
/// store configured going on once a passed preflight is there: the cell says what it needs, and
/// the log is there. With a fault proxy named, the switch is built and the cell still says it.
#[tokio::test]
async fn the_run_makes_its_request_log_and_the_cell_says_what_it_needs() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-cells");
    let out = dir.path().join("out");
    write_report(&out, &PreflightReport::default());
    let err = run_owned(&run_args(&out, &[])).await.unwrap_err();
    assert!(matches!(err, BenchError::Setup(_)), "{err}");
    assert!(out.join("requests.jsonl").is_file());
    let proxy_dir = dir.path().join("proxy");
    let (control, proxy_dir) = (dir.path().join("control"), proxy_dir.to_str().unwrap());
    let with_proxy = run_args(
        &out,
        &[
            "--fault-control",
            control.to_str().unwrap(),
            "--fault-result-dir",
            proxy_dir,
        ],
    );
    let err = run_owned(&with_proxy).await.unwrap_err();
    assert!(matches!(err, BenchError::Setup(_)), "{err}");
}

/// Catches a run that goes on without the facts of a passed preflight: no report is an error
/// naming the file, a file that is not a report is refused, and a report with a failed check
/// stops the run naming the check.
#[tokio::test]
async fn a_run_needs_a_passed_preflight() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-run-preflight");
    let out = dir.path().join("out");
    let err = run_owned(&run_args(&out, &[])).await.unwrap_err();
    match err {
        BenchError::Io { path, .. } => assert!(path.ends_with(preflight::REPORT_FILE)),
        other => panic!("{other:?}"),
    }
    std::fs::write(out.join(preflight::REPORT_FILE), "not json").unwrap();
    let err = run_owned(&run_args(&out, &[])).await.unwrap_err();
    assert!(matches!(err, BenchError::Result(_)), "{err}");
    let failed = PreflightReport {
        checks: vec![preflight::check("load-gate", Err("too busy".to_owned()))],
        ..PreflightReport::default()
    };
    write_report(&out, &failed);
    let err = run_owned(&run_args(&out, &[])).await.unwrap_err();
    assert!(err.to_string().contains("load-gate"), "{err}");
}

/// Catches the report verb not reaching its module: an empty result directory is a table with
/// no cells, and a directory that is not there stops it.
#[tokio::test]
async fn the_report_verb_renders_a_directory_and_stops_on_a_missing_one() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-report-verb");
    let there = dir.path().to_str().unwrap();
    run(&["report", "--result-dir", there]).await.unwrap();
    let absent = dir.path().join("absent");
    let err = run(&["report", "--result-dir", absent.to_str().unwrap()])
        .await
        .unwrap_err();
    assert!(matches!(err, BenchError::Io { .. }), "{err}");
}

/// Catches a result directory that cannot be made being ignored.
#[tokio::test]
async fn a_result_directory_that_cannot_be_made_stops_the_run() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-nodir");
    let file = dir.path().join("a-file");
    std::fs::write(&file, "x").unwrap();
    let under_file = file.join("out");
    let args = run_args(&under_file, &[]);
    assert!(matches!(run_owned(&args).await, Err(BenchError::Io { .. })));
}

/// Catches usage the parser must refuse: no cell, a one-pass run (no pass left after the
/// warm-up), an unknown cell, a run with no id, a fault proxy given by one of its two flags and
/// a mode that is no fault.
#[test]
fn bad_usage_is_refused_by_the_parser() {
    let bad: [&[&str]; 7] = [
        &[
            "run",
            "--result-dir",
            "o",
            "--run-id",
            "r",
            "--git-sha",
            "g",
        ],
        &[
            "run",
            "--result-dir",
            "o",
            "--run-id",
            "r",
            "--git-sha",
            "g",
            "--cell",
            "a1",
            "--passes",
            "1",
        ],
        &[
            "run",
            "--result-dir",
            "o",
            "--run-id",
            "r",
            "--git-sha",
            "g",
            "--cell",
            "z9",
        ],
        &["run", "--result-dir", "o", "--cell", "a1", "--git-sha", "g"],
        &[
            "run",
            "--result-dir",
            "o",
            "--run-id",
            "r",
            "--git-sha",
            "g",
            "--cell",
            "a1",
            "--fault-control",
            "c",
        ],
        &[
            "run",
            "--result-dir",
            "o",
            "--run-id",
            "r",
            "--git-sha",
            "g",
            "--cell",
            "a1",
            "--fault-result-dir",
            "d",
        ],
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

/// The arguments with the value of `--campaign-ledger` replaced.
fn with_ledger(args: &[String], ledger: &std::path::Path) -> Vec<String> {
    let mut out = args.to_vec();
    let at = out.iter().position(|a| a == "--campaign-ledger").unwrap();
    out[at + 1] = ledger.to_str().unwrap().to_owned();
    out
}

/// Catches a campaign that starts or is reset by accident: a ledger that is not there is a
/// refusal without `--new-campaign` and nothing is made; one inside the result directory is
/// refused; `--new-campaign` makes it, and then refuses to make it again, so a flag left in a
/// command line cannot reset the count. The run itself goes on to the cell, which says it has no
/// store.
#[tokio::test]
async fn a_campaign_ledger_is_begun_once_and_never_by_accident() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-campaign");
    let out = dir.path().join("out");
    write_report(&out, &PreflightReport::default());
    let ledger = ledger_of(&out);

    let begin = run_args(&out, &[]);
    assert!(begin.contains(&"--new-campaign".to_owned()));
    let without_new: Vec<String> = begin
        .iter()
        .filter(|a| *a != "--new-campaign")
        .cloned()
        .collect();
    let err = run_owned(&without_new).await.unwrap_err();
    assert!(matches!(err, BenchError::Budget(_)), "{err}");
    assert!(err.to_string().contains("--new-campaign"), "{err}");
    assert!(!ledger.exists());

    let inside = out.join("ledger.json");
    let err = run_owned(&with_ledger(&begin, &inside)).await.unwrap_err();
    assert!(
        err.to_string().contains("outside the result directory"),
        "{err}"
    );
    assert!(!inside.exists());

    let err = run_owned(&begin).await.unwrap_err();
    assert!(matches!(err, BenchError::Setup(_)), "{err}");
    let begun = std::fs::read_to_string(&ledger).unwrap();
    assert_eq!(begun, "{\"cold_written_bytes\":0}");

    let err = run_owned(&begin).await.unwrap_err();
    assert!(err.to_string().contains("would reset it"), "{err}");
    assert_eq!(std::fs::read_to_string(&ledger).unwrap(), begun);
    let err = run_owned(&run_args(&out, &[])).await.unwrap_err();
    assert!(
        matches!(err, BenchError::Setup(_)),
        "continued without the flag: {err}"
    );
}

/// A `run` command line parsed.
fn parsed_run(extra: &[&str]) -> Result<cli::RunArgs, clap::Error> {
    let mut argv = vec![
        "tierstore-bench",
        "run",
        "--result-dir",
        "o",
        "--test-prefix",
        "tb-",
        "--run-id",
        "r",
        "--git-sha",
        "g",
        "--cell",
        "a1",
    ];
    argv.extend_from_slice(extra);
    match Cli::try_parse_from(argv)?.command {
        cli::Command::Run(args) => Ok(args),
        other => panic!("{other:?}"),
    }
}

/// Catches a run that can be started without a campaign ledger, a fault time to live that is not
/// the flag's (a fault that outlives a dead run, or one that ends under a cell), and a time to
/// live of zero, which the proxy would refuse.
#[test]
fn the_run_flags_name_the_ledger_and_the_fault_time_to_live() {
    assert!(parsed_run(&[]).is_err(), "no --campaign-ledger");
    let args = parsed_run(&["--campaign-ledger", "l"]).unwrap();
    assert!(!args.new_campaign);
    assert_eq!(args.fault_ttl_secs.get(), 300);
    assert!(args.fault_switch().is_none(), "no proxy, no switch");

    let args = parsed_run(&[
        "--campaign-ledger",
        "l",
        "--new-campaign",
        "--fault-control",
        "c",
        "--fault-result-dir",
        "d",
        "--fault-ttl-secs",
        "7",
    ])
    .unwrap();
    assert!(args.new_campaign);
    assert_eq!(
        args.fault_switch().unwrap().ttl(),
        std::time::Duration::from_secs(7)
    );
    let default = parsed_run(&[
        "--campaign-ledger",
        "l",
        "--fault-control",
        "c",
        "--fault-result-dir",
        "d",
    ])
    .unwrap();
    assert_eq!(
        default.fault_switch().unwrap().ttl(),
        tierstore_bench::faultfile::DEFAULT_TTL
    );
    assert!(parsed_run(&["--campaign-ledger", "l", "--fault-ttl-secs", "0"]).is_err());
}

/// Catches `plan` reaching a store or ignoring the prefix rule: it needs a test prefix, refuses a
/// run file whose bucket is outside it (naming the value) and, given a good one, succeeds.
#[tokio::test]
async fn plan_refuses_a_name_outside_the_prefix_and_reads_a_good_run_file() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-plan");
    let out = dir.path().join("out");
    let out = out.to_str().unwrap();
    let file = dir.path().join("run.json");
    let write = |bucket: &str| {
        std::fs::write(
            &file,
            format!(
                r#"{{"stores": {{"cold": {{"endpoint": "http://store.example:9000",
                "bucket": "{bucket}", "access_key_id": "tb-key", "secret_access_key": "s"}}}}}}"#
            ),
        )
        .unwrap();
    };
    let file = file.to_str().unwrap();

    write("tb-cold");
    run(&[
        "plan",
        "--result-dir",
        out,
        "--run-file",
        file,
        "--test-prefix",
        "tb-",
    ])
    .await
    .unwrap();
    let err = run(&["plan", "--result-dir", out, "--run-file", file])
        .await
        .unwrap_err();
    assert!(err.to_string().contains("test_prefix"), "{err}");

    write("farm-cold");
    let err = run(&[
        "plan",
        "--result-dir",
        out,
        "--run-file",
        file,
        "--test-prefix",
        "tb-",
    ])
    .await
    .unwrap_err();
    assert!(matches!(err, BenchError::Config(_)), "{err}");
    assert!(err.to_string().contains("farm-cold"), "{err}");
}
