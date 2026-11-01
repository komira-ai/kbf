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
    let ops_verbs: [&[&str]; 5] = [
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
            "provision",
            "--result-dir",
            out,
            "--test-prefix",
            "tb-",
            "--premade",
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

/// The directory the ops file of `out` names as the live tierstore's, beside `out`.
fn live_of(out: &std::path::Path) -> std::path::PathBuf {
    out.with_file_name("live-state")
}

/// Writes the ops file `run` reads first, with `live` as the live tierstore's directories.
fn write_ops_with(out: &std::path::Path, live: &[&std::path::Path]) {
    std::fs::create_dir_all(out).unwrap();
    let ops = serde_json::json!({
        "tierstore": {"binary": "/x/tierstore", "sha256": "00", "version": "0.0.37", "plan_flags": []},
        "live": {"unit": "live.service", "binary": "/x/live", "port": 9300, "dirs": live},
        "test_instance": {"unit": "test.service", "port": 9411, "dirs": ["/x/state"]}
    });
    std::fs::write(out.join("ops.json"), ops.to_string()).unwrap();
}

/// [`write_ops_with`] for a live directory beside `out`.
fn write_ops(out: &std::path::Path) {
    write_ops_with(out, &[live_of(out).as_path()]);
}

fn write_report(out: &std::path::Path, report: &PreflightReport) {
    write_ops(out);
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
    write_ops(&out);
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

/// Catches a result directory that is no directory (a file in the way, so no ops file in it)
/// being ignored.
#[tokio::test]
async fn a_result_directory_that_is_no_directory_stops_the_run() {
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
        cli::Command::Run(args) => Ok(*args),
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

/// Catches an attempt number that is not 1 unless asked for, one of 0 (no attempt `a0`), and one
/// that is not a number.
#[test]
fn the_attempt_flag_defaults_to_the_first_and_is_a_positive_number() {
    assert_eq!(parsed_run(&["--campaign-ledger", "l"]).unwrap().attempt, 1);
    let args = parsed_run(&["--campaign-ledger", "l", "--attempt", "3"]).unwrap();
    assert_eq!(args.attempt, 3);
    for bad in ["0", "-1", "two"] {
        assert!(
            parsed_run(&["--campaign-ledger", "l", "--attempt", bad]).is_err(),
            "{bad}"
        );
    }
}

/// What `run` says when it is refused for `live` directories, and whether it made anything:
/// the result directory `out` (made by the caller only as far as its ops file), the request log
/// and the ledger must not exist.
async fn refused_for_live(out: &std::path::Path, live: &[&std::path::Path]) -> String {
    write_ops_with(out, live);
    let err = run_owned(&run_args(out, &[])).await.unwrap_err();
    assert!(matches!(err, BenchError::LiveDir(_)), "{err}");
    assert!(
        !out.join("requests.jsonl").exists(),
        "the request log was made"
    );
    assert!(!ledger_of(out).exists(), "the ledger was made");
    assert!(
        !ledger_of(out).with_extension("json.lock").exists(),
        "the lock was made"
    );
    err.to_string()
}

/// Catches a run that writes in or around the live tierstore's directories: a result directory
/// that is, holds or lies inside one, a ledger inside one, a ledger lock that is one, the same
/// through a link or a `..`, and an ops file that lists none. Each is refused before the request
/// log, the ledger or its lock is made.
#[tokio::test]
async fn a_run_stays_clear_of_the_live_directories() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-live");
    let root = dir.path();
    let out = root.join("out");

    let text = refused_for_live(&out, &[&out]).await;
    assert!(
        text.contains("result directory") && text.contains("is the live directory"),
        "{text}"
    );
    let text = refused_for_live(&out, &[root]).await;
    assert!(
        text.contains("result directory") && text.contains("is inside"),
        "{text}"
    );
    let text = refused_for_live(&out, &[&out.join("data")]).await;
    assert!(
        text.contains("result directory") && text.contains("contains"),
        "{text}"
    );

    let in_live = root.join("live-ledgers");
    std::fs::create_dir_all(&in_live).unwrap();
    write_ops_with(&out, &[&in_live]);
    let args = with_ledger(&run_args(&out, &[]), &in_live.join("l.json"));
    let err = run_owned(&args).await.unwrap_err();
    assert!(err.to_string().contains("campaign ledger"), "{err}");
    assert!(matches!(err, BenchError::LiveDir(_)), "{err}");
    assert!(!in_live.join("l.json").exists() && !out.join("requests.jsonl").exists());

    let lock = ledger_of(&out).with_extension("json.lock");
    let text = refused_for_live(&out, &[&lock]).await;
    assert!(text.contains("campaign ledger lock"), "{text}");

    let text = refused_for_live(&out, &[]).await;
    assert!(text.contains("live.dirs"), "{text}");

    let live = root.join("live-state");
    std::fs::create_dir_all(&live).unwrap();
    std::fs::create_dir_all(root.join("work")).unwrap();
    let dotted = root.join("work").join("..").join("live-state").join("out");
    let text = refused_for_live(&dotted, &[&live]).await;
    assert!(text.contains("is inside"), "{text}");

    #[cfg(unix)]
    {
        let linked = root.join("linked-out");
        std::os::unix::fs::symlink(&live, &linked).unwrap();
        let text = refused_for_live(&linked, &[&live]).await;
        assert!(
            text.contains("result directory") && text.contains("is the live"),
            "{text}"
        );
    }
}

/// Catches an ops file with no `live.dirs` being read as one that lists none: the field is
/// required, and the run says the file is not an ops file.
#[tokio::test]
async fn an_ops_file_without_live_dirs_is_not_an_ops_file() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-nolivedirs");
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let ops = serde_json::json!({
        "tierstore": {"binary": "/x/tierstore", "sha256": "00", "version": "0.0.37", "plan_flags": []},
        "live": {"unit": "live.service", "binary": "/x/live", "port": 9300},
        "test_instance": {"unit": "test.service", "port": 9411, "dirs": ["/x/state"]}
    });
    std::fs::write(out.join("ops.json"), ops.to_string()).unwrap();
    let err = run_owned(&run_args(&out, &[])).await.unwrap_err();
    assert!(matches!(err, BenchError::Ops(_)), "{err}");
    assert!(err.to_string().contains("not an ops file"), "{err}");
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

/// Catches the `--arms` flag not reaching the run: a comma list is the filter, no flag is no
/// filter, and a family no selected cell has is refused before anything is read (the result
/// directory here does not exist, so any other error would be about it).
#[tokio::test]
async fn the_arms_flag_is_a_filter_and_a_typo_is_refused_first() {
    assert_eq!(
        parsed_run(&["--campaign-ledger", "l"])
            .unwrap()
            .arms_filter(),
        None
    );
    let args = parsed_run(&["--campaign-ledger", "l", "--arms", "hot,instance"]).unwrap();
    assert_eq!(
        args.arms_filter(),
        Some(vec!["hot".to_owned(), "instance".to_owned()])
    );
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-arms-typo");
    let out = dir.path().join("out");
    let err = run_owned(&run_args(&out, &["--arms", "hot,nope"]))
        .await
        .unwrap_err();
    assert!(matches!(err, BenchError::Setup(_)), "{err}");
    assert!(err.to_string().contains("nope"), "{err}");
}
