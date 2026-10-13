//! The `kbf-bench` binary end to end, against a kbf-server in this process.
//!
//! Catches: the table or the JSON not written, or not the report of the run; the exit
//! status not 0 for a complete run, 1 for an incomplete one, 2 for one that could not
//! start (with the cause named); `read` without the upload's seed; no progress line.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

/// Runs the binary off the test's runtime, so the server in this process answers it.
async fn run(args: Vec<String>) -> Output {
    tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_kbf-bench"))
            .args(args)
            .output()
            .expect("run kbf-bench")
    })
    .await
    .expect("join")
}

fn args(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-bench-cli")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch");
    dir
}

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).expect("UTF-8")
}

const SMALL: [&str; 8] = [
    "--files",
    "50",
    "--empty",
    "2",
    "--dirs",
    "3",
    "--total-bytes",
    "100KB",
];

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upload_then_read_writes_the_table_and_the_json() {
    let server = support::Server::start("cli");
    let dir = scratch("upload");
    let json = dir.join("upload.json");
    let cas = format!("http://{}", server.reapi);
    let mut upload = args(&[
        "upload",
        "--cas",
        &cas,
        "--seed",
        "7",
        "--progress-secs",
        "1",
    ]);
    upload.extend(args(&SMALL));
    upload.extend(args(&["--json", json.to_str().expect("UTF-8")]));
    let out = run(upload).await;
    assert!(
        out.status.success(),
        "{}{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    let table = text(&out.stdout);
    assert!(
        table.starts_with("upload bytestream, 64 in flight, find-missing off, seed 7\n"),
        "{table}"
    );
    assert!(
        table.contains("tree: 50 files (2 empty, 0 skipped), 3 dirs, 100000 bytes, 50 blobs\n"),
        "{table}"
    );
    assert!(table.contains("\nwrite               50 "), "{table}");
    assert!(
        text(&out.stderr).contains("kbf-bench: 0.0 s: 0/50 requests, 0/50 blobs"),
        "{}",
        text(&out.stderr)
    );

    let report: Value = serde_json::from_slice(&std::fs::read(&json).expect("json")).expect("JSON");
    assert_eq!(report["command"], "upload");
    assert_eq!(report["method"], "bytestream");
    assert_eq!(report["seed"], 7);
    assert_eq!(report["blobs_done"], 50);
    assert_eq!(report["tree"]["file_bytes"], 100_000);
    assert_eq!(report["requests"]["write"]["count"], 50);
    assert!(report["requests"]["write"]["p99_ms"].as_f64().expect("p99") > 0.0);
    assert_eq!(report["timed_out"], false);

    let mut read = args(&[
        "read",
        "--cas",
        &cas,
        "--seed",
        "7",
        "--progress-secs",
        "0",
        "--method",
        "batch",
    ]);
    read.extend(args(&SMALL));
    let out = run(read).await;
    assert!(
        out.status.success(),
        "{}{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    assert!(
        text(&out.stdout).starts_with("read batch, 64 in flight"),
        "{}",
        text(&out.stdout)
    );
    assert_eq!(text(&out.stderr), "", "no progress line at 0");

    let mut other_seed = args(&["read", "--cas", &cas, "--seed", "8", "--progress-secs", "0"]);
    other_seed.extend(args(&SMALL));
    let out = run(other_seed).await;
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    assert!(
        text(&out.stdout).contains("\nfailed: 49\n"),
        "{}",
        text(&out.stdout)
    );
}

/// The clock seed, a directory tree, FindMissingBlobs on and a deadline that does not
/// pass: a complete run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upload_of_a_directory_with_a_clock_seed() {
    let server = support::Server::start("cli-dir");
    let dir = scratch("from-dir");
    std::fs::write(dir.join("file"), b"contents").expect("write");
    let cas = format!("http://{}", server.reapi);
    let words = [
        "upload",
        "--cas",
        &cas,
        "--from-dir",
        dir.to_str().expect("UTF-8"),
        "--find-missing",
        "on",
        "--timeout-secs",
        "60",
        "--progress-secs",
        "0",
    ];
    let out = run(args(&words)).await;
    assert!(
        out.status.success(),
        "{}{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    let table = text(&out.stdout);
    assert!(table.contains("find-missing on, seed "), "{table}");
    assert!(
        table.contains("tree: 1 files (0 empty, 0 skipped), 0 dirs, 24 bytes, 2 blobs\n"),
        "{table}"
    );
    assert!(!table.contains(", seed 0\n"), "{table}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_that_cannot_start_exits_2_and_says_why() {
    let dir = scratch("refused");
    let json = dir.join("no-such-dir").join("report.json");
    let server = support::Server::start("cli-refused");
    let cas = format!("http://{}", server.reapi);
    let cases = [
        (args(&["read", "--cas", &cas]), "read needs the --seed"),
        (
            args(&["upload", "--cas", &cas, "--files", "1", "--empty", "2"]),
            "no tree has 1 files",
        ),
        (
            args(&[
                "upload",
                "--cas",
                "ftp://x",
                "--files",
                "0",
                "--empty",
                "0",
                "--total-bytes",
                "0",
            ]),
            "--cas \"ftp://x\"",
        ),
        (
            args(&[
                "upload",
                "--cas",
                &cas,
                "--files",
                "0",
                "--empty",
                "0",
                "--total-bytes",
                "0",
                "--progress-secs",
                "0",
                "--json",
                json.to_str().expect("UTF-8"),
            ]),
            "--json ",
        ),
    ];
    for (words, why) in cases {
        let out = run(words).await;
        assert_eq!(out.status.code(), Some(2), "{why}");
        assert!(
            text(&out.stderr).starts_with("kbf-bench: "),
            "{}",
            text(&out.stderr)
        );
        assert!(text(&out.stderr).contains(why), "{}", text(&out.stderr));
    }
}
