//! `kbf-bake run` and `tag-message` through the command line, against a farm that is a local
//! server of canned metrics text, with the real clock (bakes of a second).

mod common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use common::{Farm, scratch};
use kbf_bake::cli::{EXIT_EXTENDED, EXIT_GREEN, EXIT_RED, EXIT_USAGE, execute};
use kbf_bake::state::State;
use kbf_gate::{ResultDoc, SetHash, Status};

const SUMS: &[u8] = b"0123  kbf-linux-x86_64.tar.gz\n";

/// A farm: serves whatever text is in the cell, on a port of its own.
struct Server {
    url: String,
    body: Arc<Mutex<String>>,
}

fn serve(text: String) -> Server {
    serve_with(text, || {})
}

/// A farm that runs `on_request` when a scrape arrives, before it answers.
fn serve_with(text: String, on_request: impl Fn() + Send + 'static) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/metrics", listener.local_addr().unwrap());
    let body = Arc::new(Mutex::new(text));
    let shared = Arc::clone(&body);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            on_request();
            let body = shared.lock().unwrap().clone();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    Server { url, body }
}

struct Files {
    state: PathBuf,
    verdict: PathBuf,
    sums: PathBuf,
    log: PathBuf,
}

fn files(name: &str) -> Files {
    let dir = scratch(name);
    std::fs::write(dir.join("SHA256SUMS"), SUMS).unwrap();
    std::fs::write(dir.join("updater.json"), br#"{"steps":[],"restarts":[]}"#).unwrap();
    Files {
        state: dir.join("state.json"),
        verdict: dir.join("gamma-verdict.json"),
        sums: dir.join("SHA256SUMS"),
        log: dir.join("updater.json"),
    }
}

fn run_args(f: &Files, url: &str, extra: &[&str]) -> Vec<String> {
    let mut a: Vec<String> = [
        "kbf-bake",
        "run",
        "--bake-seconds",
        "1",
        "--interval-seconds",
        "1",
        "--baseline-error-ratio",
        "0",
        "--baseline-queue-p99",
        "0.5",
        "--commit",
        &"ab".repeat(20),
        "--run-id",
        "bake-1",
    ]
    .map(String::from)
    .to_vec();
    for (flag, value) in [
        ("--state", f.state.to_str().unwrap()),
        ("--verdict", f.verdict.to_str().unwrap()),
        ("--sha256sums", f.sums.to_str().unwrap()),
        ("--updater-log", f.log.to_str().unwrap()),
        ("--metrics-url", url),
    ] {
        a.push(flag.to_owned());
        a.push(value.to_owned());
    }
    a.extend(extra.iter().map(|s| (*s).to_owned()));
    a
}

fn exec(args: &[String]) -> (u8, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = execute(args.iter().map(Into::into).collect(), &mut out, &mut err);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

const WAIVE: [&str; 5] = [
    "--waive-min-load",
    "--waive-by",
    "operator",
    "--waive-reason",
    "rehearsal",
];

fn verdict(f: &Files) -> ResultDoc {
    ResultDoc::from_bytes(&std::fs::read(&f.verdict).unwrap()).unwrap()
}

/// A healthy farm with a recorded waiver ends green; the verdict carries the waiver, the commit and
/// the set hash of the sums file.
#[test]
fn a_green_run_records_its_waiver_and_the_set_hash() {
    let f = files("cli-green");
    let farm = serve(Farm::healthy(0.0).text());
    let (code, out, err) = exec(&run_args(&f, &farm.url, &WAIVE));
    assert_eq!((code, err.as_str()), (EXIT_GREEN, ""), "{out}");
    assert!(out.starts_with("kbf-bake: green"), "{out}");
    let doc = verdict(&f);
    assert!(doc.passed());
    assert_eq!(doc.set_hash, SetHash::of_sums(SUMS));
    assert_eq!(doc.commit.as_str(), "ab".repeat(20));
    assert_eq!(doc.waivers.len(), 1);
    let w = &doc.waivers[0];
    assert_eq!(
        (w.what.as_str(), w.by.as_str(), w.reason.as_str()),
        ("min-load", "operator", "rehearsal")
    );
    assert_eq!(
        State::load(&f.state).unwrap().unwrap().ended,
        Some(Status::Green)
    );
}

/// Catches a waiver without a recorded reason: no operator, no reason or a blank one refuses the
/// command before anything is written, and a waiver detail without the flag is a usage error.
#[test]
fn a_waiver_needs_an_operator_and_a_recorded_reason() {
    let farm = serve(Farm::healthy(0.0).text());
    for (name, extra) in [
        ("no-detail", vec!["--waive-min-load"]),
        (
            "no-reason",
            vec!["--waive-min-load", "--waive-by", "operator"],
        ),
        (
            "blank-reason",
            vec![
                "--waive-min-load",
                "--waive-by",
                "operator",
                "--waive-reason",
                "  ",
            ],
        ),
        (
            "no-operator",
            vec!["--waive-min-load", "--waive-reason", "holiday"],
        ),
    ] {
        let f = files(&format!("cli-waiver-{name}"));
        let (code, _, err) = exec(&run_args(&f, &farm.url, &extra));
        assert_eq!(code, EXIT_USAGE, "{name}");
        assert!(
            err.contains("a waiver needs --waive-by and --waive-reason"),
            "{name}: {err}"
        );
        assert!(
            !f.state.exists() && !f.verdict.exists(),
            "{name}: nothing is written"
        );
    }
    let f = files("cli-waiver-orphan");
    let (code, _, err) = exec(&run_args(&f, &farm.url, &["--waive-reason", "holiday"]));
    assert_eq!(code, EXIT_USAGE);
    assert!(err.contains("--waive-min-load"), "{err}");
}

/// Catches thin usage passing, a restart that begins a new bake, and a second run that forgets the
/// first's state: the bake is extended, and a second run finds the same clock.
#[test]
fn thin_usage_extends_and_a_second_run_keeps_the_clock() {
    let f = files("cli-thin");
    let farm = serve(Farm::healthy(0.0).text());
    let (code, out, err) = exec(&run_args(&f, &farm.url, &[]));
    assert_eq!(code, EXIT_EXTENDED, "{out}");
    assert!(
        err.starts_with("PAGE: the gamma bake is extended: minimum load not met"),
        "{err}"
    );
    assert_eq!(verdict(&f).status, Status::Extended);
    let before = State::load(&f.state).unwrap().unwrap();

    std::thread::sleep(std::time::Duration::from_millis(1100));
    // A later run needs only the files and the endpoint: the identity is in the state.
    let mut again = run_args(&f, &farm.url, &[]);
    for flag in ["--commit", "--sha256sums", "--run-id"] {
        let at = again.iter().position(|x| x == flag).unwrap();
        again.drain(at..at + 2);
    }
    let (code, _, _) = exec(&again);
    assert_eq!(code, EXIT_EXTENDED);
    let after = State::load(&f.state).unwrap().unwrap();
    assert_eq!(after.bake_start, before.bake_start);
    assert_eq!(
        (after.commit, after.set_hash),
        (before.commit, before.set_hash)
    );
}

/// Catches a red bake that a restart resumes: the run after a red one reports red, writes
/// nothing, and leaves the verdict as it was.
#[test]
fn a_bake_that_ended_red_is_not_resumed() {
    let f = files("cli-red");
    let farm = serve(
        Farm {
            lost_blobs: 1.0,
            ..Farm::healthy(0.0)
        }
        .text(),
    );
    let (code, _, err) = exec(&run_args(&f, &farm.url, &WAIVE));
    assert_eq!(code, EXIT_RED);
    assert!(
        err.starts_with("PAGE: the gamma bake is red: gate `lost-blobs` is red"),
        "{err}"
    );
    let written = std::fs::read(&f.verdict).unwrap();
    *farm.body.lock().unwrap() = Farm::healthy(0.0).text();
    let (code, out, _) = exec(&run_args(&f, &farm.url, &WAIVE));
    assert_eq!(code, EXIT_RED);
    assert!(out.contains("already ended"), "{out}");
    assert_eq!(std::fs::read(&f.verdict).unwrap(), written);
}

/// Catches a state file taken for another bake: a different run id or commit is refused and the
/// state is not touched.
#[test]
fn a_state_file_of_another_bake_is_refused() {
    let f = files("cli-identity");
    let farm = serve(Farm::healthy(0.0).text());
    exec(&run_args(&f, &farm.url, &[]));
    let before = std::fs::read(&f.state).unwrap();
    let mut args = run_args(&f, &farm.url, &[]);
    let at = args.iter().position(|a| a == "bake-1").unwrap();
    args[at] = "bake-2".to_owned();
    let (code, _, err) = exec(&args);
    assert_eq!(code, EXIT_USAGE);
    assert!(err.contains("holds the bake of commit"), "{err}");
    let mut args = run_args(&f, &farm.url, &[]);
    let at = args.iter().position(|a| *a == "ab".repeat(20)).unwrap();
    args[at] = "cd".repeat(20);
    assert_eq!(exec(&args).0, EXIT_USAGE);
    // Another release set: the same commit and run, other bytes in SHA256SUMS.
    let other = f.sums.with_file_name("SHA256SUMS.other");
    std::fs::write(&other, b"4567  other.tar.gz\n").unwrap();
    let mut args = run_args(&f, &farm.url, &[]);
    let at = args
        .iter()
        .position(|a| *a == f.sums.to_str().unwrap())
        .unwrap();
    args[at] = other.to_str().unwrap().to_owned();
    let (code, _, err) = exec(&args);
    assert_eq!(code, EXIT_USAGE);
    assert!(err.contains("holds the bake of commit"), "{err}");
    assert_eq!(std::fs::read(&f.state).unwrap(), before);
}

/// Catches an evaluator that cannot reach the farm passing or going red: it extends, and pages.
#[test]
fn a_farm_that_is_down_extends_the_bake_and_pages() {
    let f = files("cli-down");
    // Port 1 has no listener; a port bound and dropped here could be taken by another test's server.
    let url = "http://127.0.0.1:1/metrics";
    let (code, _, err) = exec(&run_args(&f, url, &WAIVE));
    assert_eq!(code, EXIT_EXTENDED);
    assert!(
        err.starts_with("PAGE: the gamma bake is extended: the farm could not be read 3 times"),
        "{err}"
    );
    assert_eq!(verdict(&f).status, Status::Extended);
}

/// Catches an evaluator error that leaves the command as a usage error (exit 2, nothing decided,
/// no `PAGE:`): the state file cannot be written after the first scrape, so the verdict is sealed
/// extended and the command must say so with exit 3 and a `PAGE:` line.
#[test]
fn an_evaluator_error_extends_the_bake_and_pages() {
    let f = files("cli-evaluator-error");
    let state = f.state.clone();
    // Once the first scrape arrives the state file becomes a directory: the save cannot rename onto it.
    let farm = serve_with(Farm::healthy(0.0).text(), move || {
        let _ = std::fs::remove_file(&state);
        let _ = std::fs::create_dir(&state);
    });
    let (code, out, err) = exec(&run_args(&f, &farm.url, &WAIVE));
    assert_eq!(code, EXIT_EXTENDED, "{out}{err}");
    assert!(
        err.starts_with("PAGE: the gamma bake is extended: the evaluator failed: "),
        "{err}"
    );
    assert!(!err.contains("kbf-bake: "), "{err}");
    let doc = verdict(&f);
    assert_eq!(doc.status, Status::Extended);
    assert!(doc.note.unwrap().contains("the evaluator failed"));
}

/// Catches commands and inputs accepted that decide nothing.
#[test]
fn wrong_commands_and_inputs_are_usage_errors() {
    let f = files("cli-usage");
    let farm = serve(Farm::healthy(0.0).text());
    let missing = |flag: &str| {
        let mut a = run_args(&f, &farm.url, &[]);
        let at = a.iter().position(|x| x == flag).unwrap();
        a.drain(at..at + 2);
        a
    };
    for flag in ["--commit", "--sha256sums", "--run-id"] {
        let (code, _, err) = exec(&missing(flag));
        assert_eq!(code, EXIT_USAGE, "{flag}");
        assert!(
            err.contains("a new bake needs --handoff, or --commit, --sha256sums and --run-id"),
            "{err}"
        );
    }
    for (flag, value, want) in [
        (
            "--baseline-error-ratio",
            "-0.5",
            "--baseline-error-ratio -0.5 is not a usable baseline",
        ),
        (
            "--baseline-error-ratio",
            "NaN",
            "--baseline-error-ratio NaN is not a usable baseline",
        ),
        (
            "--baseline-queue-p99",
            "0",
            "--baseline-queue-p99 0 is not a usable baseline",
        ),
        ("--bake-seconds", "0", "must be above zero"),
        ("--interval-seconds", "0", "must be above zero"),
        ("--state", "/no/such/dir/state.json", "write state file"),
    ] {
        let mut a = run_args(&f, &farm.url, &[]);
        let at = a.iter().position(|x| x == flag).unwrap();
        a.drain(at..at + 2);
        a.push(format!("{flag}={value}"));
        let (code, _, err) = exec(&a);
        assert_eq!(code, EXIT_USAGE, "{flag} {value}");
        assert!(err.contains(want), "{flag} {value}: {err}");
    }
    for bad in [&["--no-such-flag"][..], &["--deploy-start=5"]] {
        let (code, _, _) = exec(&run_args(&f, &farm.url, bad));
        assert_eq!(code, EXIT_USAGE, "{bad:?}");
    }
    let (code, out, _) = exec(&["kbf-bake".into(), "--help".into()]);
    assert_eq!(code, EXIT_GREEN);
    assert!(out.contains("tag-message"), "{out}");
    assert_eq!(exec(&["kbf-bake".into()]).0, EXIT_USAGE);
    let mut bad_commit = run_args(&f, &farm.url, &[]);
    let at = bad_commit
        .iter()
        .position(|a| *a == "ab".repeat(20))
        .unwrap();
    bad_commit[at] = "xyz".to_owned();
    assert_eq!(exec(&bad_commit).0, EXIT_USAGE);
    let mut no_sums = run_args(&f, &farm.url, &[]);
    let at = no_sums
        .iter()
        .position(|a| a.ends_with("SHA256SUMS"))
        .unwrap();
    no_sums[at] = "/no/such/SHA256SUMS".to_owned();
    assert_eq!(exec(&no_sums).0, EXIT_USAGE);
}

/// The deploy's span and generation reach the verdict.
#[test]
fn the_deploy_window_and_generation_are_in_the_verdict() {
    let f = files("cli-deploy");
    let farm = serve(Farm::healthy(0.0).text());
    let extra: Vec<&str> = [
        &WAIVE[..],
        &[
            "--desired-generation",
            "7",
            "--deploy-start",
            "100",
            "--deploy-end",
            "900",
        ],
    ]
    .concat();
    assert_eq!(exec(&run_args(&f, &farm.url, &extra)).0, EXIT_GREEN);
    let doc = verdict(&f);
    assert_eq!(doc.desired_generation, Some(7));
    let d = doc.deploy.unwrap();
    assert_eq!((d.start, d.end), (100, Some(900)));
}

/// Catches a missing baseline passing: a bake started without the two baselines has a red error
/// gate and a red latency gate, and ends red.
#[test]
fn a_bake_without_baselines_ends_red() {
    let f = files("cli-no-baseline");
    let farm = serve(Farm::healthy(0.0).text());
    let mut a = run_args(&f, &farm.url, &WAIVE);
    for flag in ["--baseline-error-ratio", "--baseline-queue-p99"] {
        let at = a.iter().position(|x| x == flag).unwrap();
        a.drain(at..at + 2);
    }
    let (code, _, err) = exec(&a);
    assert_eq!(code, EXIT_RED);
    assert!(err.contains("`error-rate` is red"), "{err}");
}
