//! `kbf-bench noop` end to end, against fake frontends in this process: the table and the
//! JSON lines of a run, the exit status (0 every run succeeded, 1 one failed, 2 the run
//! could not start, with the cause named), and the flags refused.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

use kbf_bench::cli::{parse_arm, parse_property};
use support::front::{self, Reply, Shared, Upload, exited};

/// Runs the binary off the test's runtime, so the fakes in this process answer it.
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

fn words(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-bench-noop-cli")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch");
    dir
}

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).expect("UTF-8")
}

fn path(p: &Path) -> &str {
    p.to_str().expect("UTF-8")
}

/// Catches the flags not reaching the run (the arms, their order, the blocks, the window,
/// the seed, `--arch`, the platform), the table or the records not written, and a
/// complete run not exiting 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn noop_against_two_arms_writes_the_table_and_a_line_per_run() {
    let shared = Arc::new(Shared::default());
    let a = front::start("a", &shared, Upload::Keep, |_| Reply::Done(exited(0)));
    let c = front::start("c", &shared, Upload::Keep, |_| Reply::Done(exited(0)));
    let jsonl = scratch("two-arms").join("runs.jsonl");
    let out = run(words(&[
        "noop",
        "--arm",
        &format!("a=http://{a}"),
        "--arm",
        &format!("c=http://{c}"),
        "--serial-runs",
        "2",
        "--parallel-runs",
        "3",
        "--in-flight",
        "2",
        "--block",
        "1",
        "--seed",
        "5",
        "--arch",
        "aarch64",
        "--instance-name",
        "bench",
        "--platform",
        "OSFamily=linux",
        "--platform",
        "container-image=",
        "--jsonl",
        path(&jsonl),
    ]))
    .await;
    assert!(
        out.status.success(),
        "{}{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    let table = text(&out.stdout);
    assert!(
        table.starts_with("noop aarch64, seed 5, blocks of 1, parallel 2 in flight\n"),
        "{table}"
    );
    assert!(
        table.contains("\nc           parallel        3       0 "),
        "{table}"
    );
    assert!(table.ends_with("\nfailed: 0\n"), "{table}");
    let lines = std::fs::read_to_string(&jsonl).expect("records");
    assert_eq!(lines.lines().count(), 10);
    let ran = shared.ran.lock().expect("ran").clone();
    let serial: Vec<String> = ran[..4].iter().map(|r| r.env.join("-")).collect();
    assert_eq!(
        serial,
        [
            "5-a-serial-0",
            "5-c-serial-0",
            "5-a-serial-1",
            "5-c-serial-1"
        ]
    );
    for r in &ran {
        assert_eq!(r.request.instance_name, "bench");
        assert_eq!(
            r.program,
            kbf_bench::program::noop(kbf_bench::program::Arch::Aarch64)
        );
        let properties = &r.action.platform.as_ref().expect("platform").properties;
        let pairs: Vec<(&str, &str)> = properties
            .iter()
            .map(|p| (p.name.as_str(), p.value.as_str()))
            .collect();
        assert_eq!(pairs, [("OSFamily", "linux"), ("container-image", "")]);
    }
}

/// Catches a failed run exiting 0, and the defaults not the design's (300 and 300 runs,
/// 64 in flight, blocks of 20, the program of this machine, a seed from the clock).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_run_exits_1_and_the_defaults_are_the_gates() {
    let shared = Arc::new(Shared::default());
    let a = front::start("a", &shared, Upload::Keep, |ran| {
        match ran.env[3].as_str() {
            "7" => Reply::Done(exited(2)),
            _ => Reply::Done(exited(0)),
        }
    });
    let jsonl = scratch("defaults").join("runs.jsonl");
    let out = run(words(&[
        "noop",
        "--arm",
        &format!("a=http://{a}"),
        "--jsonl",
        path(&jsonl),
    ]))
    .await;
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    let table = text(&out.stdout);
    let arch = std::env::consts::ARCH;
    assert!(table.starts_with(&format!("noop {arch}, seed ")), "{table}");
    assert!(
        table.contains(", blocks of 20, parallel 64 in flight\n"),
        "{table}"
    );
    assert!(
        table.contains("\na           serial        300       1 "),
        "{table}"
    );
    assert!(
        table.contains("\na           parallel      300       1 "),
        "{table}"
    );
    assert!(
        table.ends_with(
            "\nfailed: 2\n  a serial run 7: ./noop exited 2\n  a parallel run 7: ./noop exited 2\n"
        ),
        "{table}"
    );
    assert_eq!(
        std::fs::read_to_string(&jsonl)
            .expect("records")
            .lines()
            .count(),
        600
    );
    assert!(!table.contains(" seed 0,"), "{table}");
    let host = kbf_bench::program::Arch::from_name(arch).expect("a machine kbf-bench writes for");
    let ran = shared.ran.lock().expect("ran").clone();
    assert!(
        ran.iter()
            .all(|r| r.program == kbf_bench::program::noop(host))
    );
    assert!(ran.iter().all(|r| r.request.instance_name.is_empty()));
    assert!(
        ran.iter().all(|r| r.action.platform.is_none()),
        "no --platform, no platform"
    );
}

/// Catches a run that cannot start exiting other than 2, or without its cause named: an
/// arm named twice, an arm's URL refused, an arm's TLS file missing, a `--jsonl` that
/// cannot be made, an arm refusing the blobs; and the flags clap refuses.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_noop_that_cannot_start_exits_2_and_says_why() {
    let shared = Arc::new(Shared::default());
    let full = front::start("a", &shared, Upload::RefuseCall, |_| Reply::Done(exited(0)));
    let dir = scratch("refused");
    let jsonl = dir.join("runs.jsonl");
    let missing = dir.join("no-such-dir").join("runs.jsonl");
    let full = format!("a=http://{full}");
    let cases = [
        (
            words(&[
                "noop",
                "--arm",
                "a=http://x",
                "--arm",
                "a=http://y",
                "--jsonl",
                path(&jsonl),
            ]),
            "kbf-bench: --arm a: given twice\n".to_owned(),
        ),
        (
            words(&["noop", "--arm", "a=ftp://x", "--jsonl", path(&jsonl)]),
            "kbf-bench: --arm a: --cas \"ftp://x\": must be an http:// or https:// URL\n"
                .to_owned(),
        ),
        (
            words(&[
                "noop",
                "--arm",
                "a=https://x",
                "--ca-cert",
                path(&dir.join("none.pem")),
                "--jsonl",
                path(&jsonl),
            ]),
            format!(
                "kbf-bench: --arm a: --ca-cert: read {}: No such file or directory (os error 2)\n",
                path(&dir.join("none.pem"))
            ),
        ),
        (
            words(&["noop", "--arm", &full, "--jsonl", path(&missing)]),
            format!(
                "kbf-bench: --jsonl {}: No such file or directory (os error 2)\n",
                path(&missing)
            ),
        ),
        (
            words(&["noop", "--arm", &full, "--jsonl", path(&jsonl)]),
            "kbf-bench: sending the actions to arm a: BatchUpdateBlobs: ResourceExhausted: the \
             fake is full\n"
                .to_owned(),
        ),
    ];
    for (args, why) in cases {
        let out = run(args).await;
        assert_eq!(out.status.code(), Some(2), "{why}");
        assert_eq!(text(&out.stderr), why);
        assert_eq!(text(&out.stdout), "");
    }
    for (flag, value, why) in [
        ("--arm", "a", "\"a\" is not NAME=URL"),
        ("--platform", "=x", "\"=x\" is not NAME=VALUE"),
        ("--block", "0", "0 is not in 1.."),
        ("--in-flight", "0", "0 is not in 1.."),
        (
            "--arch",
            "arm64",
            "invalid value 'arm64' for '--arch <ARCH>'",
        ),
    ] {
        let mut args = words(&["noop", "--arm", "a=http://x", "--jsonl", path(&jsonl)]);
        args.extend(words(&[flag, value]));
        let out = run(args).await;
        assert_eq!(out.status.code(), Some(2), "{flag} {value}");
        assert!(text(&out.stderr).contains(why), "{}", text(&out.stderr));
    }
    let out = run(words(&["noop", "--jsonl", path(&jsonl)])).await;
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("--arm <ARMS>"),
        "{}",
        text(&out.stderr)
    );
}

/// Catches an arm or a property read wrong: the name and the URL split at the first `=`,
/// a name of letters, digits, `-` and `_` only, and neither part empty but a property's
/// value; each refusal names what was given.
#[test]
fn arms_and_properties_are_name_equals_value() {
    let pair = |a: &str, b: &str| Ok((a.to_owned(), b.to_owned()));
    assert_eq!(
        parse_arm("arm-c_2=https://h:1/x=y"),
        pair("arm-c_2", "https://h:1/x=y")
    );
    for bad in ["a", "=http://x", "a=", "a.b=http://x", "a b=http://x"] {
        assert_eq!(
            parse_arm(bad),
            Err(format!(
                "{bad:?} is not NAME=URL (a name of letters, digits, - and _)"
            ))
        );
    }
    assert_eq!(parse_property("OSFamily=linux"), pair("OSFamily", "linux"));
    assert_eq!(parse_property("image="), pair("image", ""));
    assert_eq!(parse_property("a=b=c"), pair("a", "b=c"));
    for bad in ["x", "=x"] {
        assert_eq!(
            parse_property(bad),
            Err(format!("{bad:?} is not NAME=VALUE"))
        );
    }
}
