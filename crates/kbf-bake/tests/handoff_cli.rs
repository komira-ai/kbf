//! `kbf-bake run --handoff`, the automatic rollback behind `--auto-rollback`, and the `sample` and
//! `baseline` verbs, through the command line against local servers.

mod common;

use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use common::{Farm, api, api_with, scratch};
use kbf_bake::baseline::{BASELINE_SCHEMA, BaselineDoc};
use kbf_bake::cli::{EXIT_EXTENDED, EXIT_GREEN, EXIT_NO_BASELINE, EXIT_RED, EXIT_USAGE, execute};
use kbf_bake::handoff::{HANDOFF_SCHEMA, Handoff};
use kbf_bake::ledger::Ledger;
use kbf_bake::state::State;
use kbf_bake::table::BASELINE_SECS;
use kbf_gate::{CommitId, ResultDoc, SetHash, Window};

const NEW: &str = "ab";
const OLD: &str = "cd";

struct Files {
    dir: PathBuf,
    state: PathBuf,
    verdict: PathBuf,
    handoff: PathBuf,
    baseline: PathBuf,
    token: PathBuf,
    log: PathBuf,
}

fn handoff(reversible: bool) -> Handoff {
    Handoff {
        schema: HANDOFF_SCHEMA,
        commit: CommitId::parse(&NEW.repeat(20)).unwrap(),
        set_hash: SetHash::of_sums(b"sums\n"),
        run_id: "bake-7".to_owned(),
        deploy_start: 1_000,
        deploy_end: Some(1_600),
        desired_generation: Some(9),
        reversible,
        previous_commit: Some(CommitId::parse(&OLD.repeat(20)).unwrap()),
    }
}

fn files(name: &str, reversible: bool) -> Files {
    let dir = scratch(name);
    let f = Files {
        state: dir.join("state.json"),
        verdict: dir.join("gamma-verdict.json"),
        handoff: dir.join("handoff.json"),
        baseline: dir.join("baseline.json"),
        token: dir.join("token"),
        log: dir.join("updater.json"),
        dir,
    };
    std::fs::write(&f.handoff, handoff(reversible).to_bytes().unwrap()).unwrap();
    BaselineDoc {
        schema: BASELINE_SCHEMA,
        taken_at: 1_000,
        error_ratio: Some(0.0),
        queue_p99: Some(0.5),
        missing: vec![],
    }
    .save(&f.baseline)
    .unwrap();
    std::fs::write(&f.token, "s3cret\n").unwrap();
    std::fs::write(&f.log, br#"{"steps":[],"restarts":[]}"#).unwrap();
    f
}

fn s(p: &Path) -> String {
    p.to_str().unwrap().to_owned()
}

/// `kbf-bake run` of a one second bake on the hand-off and baseline files.
fn run_args(f: &Files, metrics: &str, extra: &[&str]) -> Vec<String> {
    let mut a: Vec<String> = [
        "kbf-bake",
        "run",
        "--bake-seconds",
        "1",
        "--interval-seconds",
        "1",
        "--state",
        &s(&f.state),
        "--verdict",
        &s(&f.verdict),
        "--updater-log",
        &s(&f.log),
        "--handoff",
        &s(&f.handoff),
        "--baseline",
        &s(&f.baseline),
        "--metrics-url",
        &format!("{metrics}/metrics"),
    ]
    .map(String::from)
    .to_vec();
    a.extend(extra.iter().map(|x| (*x).to_owned()));
    a
}

fn rollback_flags(f: &Files, api: &str) -> Vec<String> {
    vec![
        "--auto-rollback".to_owned(),
        "--rollback-url".to_owned(),
        api.to_owned(),
        "--rollback-token-file".to_owned(),
        s(&f.token),
    ]
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

fn run_with_rollback(
    f: &Files,
    farm: &Farm,
    api_url: &str,
    extra: &[&str],
) -> (u8, String, String) {
    let (metrics, _) = api("200 OK", &farm.text());
    let mut a = run_args(f, &metrics, extra);
    a.extend(rollback_flags(f, api_url));
    exec(&a)
}

fn broken() -> Farm {
    let mut farm = Farm::healthy(0.0);
    farm.lost_blobs = 1.0;
    farm
}

fn no_request(rx: &Receiver<String>) {
    assert!(
        rx.recv_timeout(Duration::from_millis(300)).is_err(),
        "the API was called"
    );
}

/// Catches a reversible release not rolled back on a measured hard red: the API gets one POST
/// offering the previous commit, compare-and-set on the generation the deploy set, with the token
/// from the file; the run still ends red and pages, and the state says what was offered.
#[test]
fn a_hard_red_of_a_reversible_release_offers_the_previous_commit() {
    let f = files("rb-offer", true);
    let (url, rx) = api("200 OK", "{}");
    let (code, out, err) = run_with_rollback(&f, &broken(), &url, &[]);
    assert_eq!(code, EXIT_RED, "{err}");
    assert!(
        out.contains(&format!(
            "ROLLBACK: offered {} over generation 9",
            OLD.repeat(20)
        )),
        "{out}"
    );
    assert!(err.contains("PAGE:"), "{err}");
    let req = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(req.starts_with("POST /v1/update/desired"), "{req}");
    assert!(
        req.to_ascii_lowercase()
            .contains("authorization: bearer s3cret"),
        "{req}"
    );
    let body: serde_json::Value =
        serde_json::from_str(req.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(body["commit"], OLD.repeat(20));
    assert_eq!(body["expect_generation"], 9);
    no_request(&rx);
    let state = State::load(&f.state).unwrap().unwrap();
    assert!(state.rollback.unwrap().starts_with("offered "));
    let doc = ResultDoc::from_bytes(&std::fs::read(&f.verdict).unwrap()).unwrap();
    assert!(!doc.passed());
    assert_eq!(doc.desired_generation, Some(9));
}

/// Catches a rollback of an irreversible release: the same red pages, says it offered nothing, and
/// the API is never called.
#[test]
fn an_irreversible_release_pages_and_calls_nothing() {
    let f = files("rb-irreversible", false);
    let (url, rx) = api("200 OK", "{}");
    let (code, out, err) = run_with_rollback(&f, &broken(), &url, &[]);
    assert_eq!(code, EXIT_RED);
    assert!(
        err.contains("ROLLBACK: not offered: the release is not reversible"),
        "{err}"
    );
    assert!(err.contains("PAGE:"), "{err}");
    assert!(!out.contains("offered "), "{out}");
    no_request(&rx);
    let state = State::load(&f.state).unwrap().unwrap();
    assert!(state.rollback.unwrap().starts_with("not offered"));
}

/// Catches the flag defaulting on: without `--auto-rollback` the same red calls nothing and says
/// nothing about a rollback.
#[test]
fn without_the_flag_there_is_no_rollback() {
    let f = files("rb-off", true);
    let (url, rx) = api("200 OK", "{}");
    let (metrics, _) = api("200 OK", &broken().text());
    let (code, out, err) = exec(
        &run_args(&f, &metrics, &["--rollback-url", &url])
            .into_iter()
            .collect::<Vec<_>>(),
    );
    // `--rollback-url` without `--auto-rollback` is refused outright.
    assert_eq!(code, EXIT_USAGE, "{out}{err}");
    let (code, out, err) = exec(&run_args(&f, &metrics, &[]));
    assert_eq!(code, EXIT_RED);
    assert!(
        !out.contains("ROLLBACK") && !err.contains("ROLLBACK"),
        "{out}{err}"
    );
    no_request(&rx);
    assert_eq!(State::load(&f.state).unwrap().unwrap().rollback, None);
}

/// Catches a rollback on a red that is only a missing baseline (a gate red for want of a number is
/// no evidence the release is bad): it pages and offers nothing.
#[test]
fn a_missing_baseline_does_not_roll_back() {
    let f = files("rb-nobaseline", true);
    let (url, rx) = api("200 OK", "{}");
    let (metrics, _) = api("200 OK", &Farm::healthy(0.0).text());
    let mut args: Vec<String> = run_args(&f, &metrics, &[]);
    let at = args.iter().position(|a| a == "--baseline").unwrap();
    args.drain(at..=at + 1);
    args.extend(rollback_flags(&f, &url));
    let (code, _, err) = exec(&args);
    assert_eq!(code, EXIT_RED, "{err}");
    assert!(
        err.contains("ROLLBACK: not offered: the red gate had no measurement"),
        "{err}"
    );
    no_request(&rx);
}

/// Catches a bake that does not end red calling the API: a green one (the minimum load waived) and
/// an extended one (thin usage) offer nothing, with the flag on.
#[test]
fn green_and_extended_bakes_never_roll_back() {
    let f = files("rb-green", true);
    let (url, rx) = api("200 OK", "{}");
    let waive = [
        "--waive-min-load",
        "--waive-by",
        "op",
        "--waive-reason",
        "rehearsal",
    ];
    let (code, out, err) = run_with_rollback(&f, &Farm::healthy(0.0), &url, &waive);
    assert_eq!(code, EXIT_GREEN, "{err}");
    assert!(!out.contains("ROLLBACK") && !err.contains("ROLLBACK"));
    let f = files("rb-extended", true);
    let (code, _, err) = run_with_rollback(&f, &Farm::healthy(0.0), &url, &[]);
    assert_eq!(code, EXIT_EXTENDED, "{err}");
    assert!(!err.contains("ROLLBACK"), "{err}");
    no_request(&rx);
}

/// Catches a retry of a refused offer and a refusal read as success: a 409 is reported as failed,
/// not retried, and recorded; the run is still red.
#[test]
fn a_refused_offer_is_reported_and_not_retried() {
    let f = files("rb-409", true);
    let (url, rx) = api("409 Conflict", "{}");
    let (code, _, err) = run_with_rollback(&f, &broken(), &url, &[]);
    assert_eq!(code, EXIT_RED);
    assert!(err.contains("ROLLBACK: failed, not retried"), "{err}");
    assert!(err.contains("409"), "{err}");
    assert!(rx.recv_timeout(Duration::from_secs(5)).is_ok());
    no_request(&rx);
    assert!(
        State::load(&f.state)
            .unwrap()
            .unwrap()
            .rollback
            .unwrap()
            .starts_with("failed")
    );
}

/// Catches a token file that is missing or empty sending a request without credentials.
#[test]
fn an_unreadable_or_empty_token_calls_nothing() {
    for (name, token) in [("rb-notoken", None), ("rb-emptytoken", Some("  \n"))] {
        let f = files(name, true);
        match token {
            None => std::fs::remove_file(&f.token).unwrap(),
            Some(t) => std::fs::write(&f.token, t).unwrap(),
        }
        let (url, rx) = api("200 OK", "{}");
        let (code, _, err) = run_with_rollback(&f, &broken(), &url, &[]);
        assert_eq!(code, EXIT_RED, "{name}");
        assert!(
            err.contains("ROLLBACK: failed, not retried"),
            "{name}: {err}"
        );
        no_request(&rx);
    }
}

/// Catches flag combinations that would leave a rollback blind: `--auto-rollback` without the
/// hand-off (reversibility unknown), the URL or the token file; the hand-off together with a flag it
/// replaces; the baseline file with a baseline flag. Each is a usage error before anything is written.
#[test]
fn contradictory_or_incomplete_flags_are_refused() {
    let f = files("rb-flags", true);
    let (metrics, _) = api("200 OK", &Farm::healthy(0.0).text());
    let base = run_args(&f, &metrics, &[]);
    let without = |flag: &str| {
        let mut a = base.clone();
        let at = a.iter().position(|x| x == flag).unwrap();
        a.drain(at..=at + 1);
        a
    };
    let with = |extra: &[&str]| {
        let mut a = base.clone();
        a.extend(extra.iter().map(|x| (*x).to_owned()));
        a
    };
    let url = "http://127.0.0.1:1";
    let tok = s(&f.token);
    let cases: Vec<(&str, Vec<String>)> = vec![
        ("no handoff", {
            let mut a = without("--handoff");
            a.extend([
                "--commit".into(),
                NEW.repeat(20),
                "--run-id".into(),
                "r".into(),
                "--sha256sums".into(),
                s(&f.dir.join("none")),
            ]);
            a.extend([
                "--auto-rollback".into(),
                "--rollback-url".into(),
                url.into(),
                "--rollback-token-file".into(),
                tok.clone(),
            ]);
            a
        }),
        (
            "no url",
            with(&["--auto-rollback", "--rollback-token-file", &tok]),
        ),
        (
            "no token",
            with(&["--auto-rollback", "--rollback-url", url]),
        ),
        ("handoff and commit", with(&["--commit", &NEW.repeat(20)])),
        ("handoff and run id", with(&["--run-id", "x"])),
        (
            "handoff and generation",
            with(&["--desired-generation", "3"]),
        ),
        (
            "baseline and flag",
            with(&["--baseline-error-ratio", "0.1"]),
        ),
        ("url alone", with(&["--rollback-url", url])),
    ];
    for (name, args) in cases {
        let (code, _, err) = exec(&args);
        assert_eq!(code, EXIT_USAGE, "{name}: {err}");
    }
    assert!(
        !f.state.exists() && !f.verdict.exists(),
        "nothing was written"
    );
}

/// Catches the hand-off not reaching the bake: its generation, deploy span, reversibility and
/// previous commit are in the state and the verdict, the baseline file's values are the baseline,
/// and a second hand-off for another commit cannot take over the state file.
#[test]
fn the_handoff_and_the_baseline_file_reach_the_state_and_the_verdict() {
    let f = files("handoff-state", true);
    let (metrics, _) = api("200 OK", &Farm::healthy(0.0).text());
    let waive = [
        "--waive-min-load",
        "--waive-by",
        "op",
        "--waive-reason",
        "rehearsal",
    ];
    let (code, _, err) = exec(&run_args(&f, &metrics, &waive));
    assert_eq!(code, EXIT_GREEN, "{err}");
    let st = State::load(&f.state).unwrap().unwrap();
    assert_eq!(st.commit.as_str(), NEW.repeat(20));
    assert_eq!(st.run_id, "bake-7");
    assert_eq!(st.set_hash, SetHash::of_sums(b"sums\n"));
    assert_eq!(st.desired_generation, Some(9));
    assert_eq!(
        st.deploy,
        Some(Window {
            start: 1_000,
            end: Some(1_600)
        })
    );
    assert_eq!(st.reversible, Some(true));
    assert_eq!(
        st.previous_commit.map(|c| c.as_str().to_owned()),
        Some(OLD.repeat(20))
    );
    assert_eq!(
        (st.baseline.error_ratio, st.baseline.queue_p99),
        (Some(0.0), Some(0.5))
    );
    let doc = ResultDoc::from_bytes(&std::fs::read(&f.verdict).unwrap()).unwrap();
    assert_eq!(
        doc.deploy,
        Some(Window {
            start: 1_000,
            end: Some(1_600)
        })
    );
    assert_eq!(doc.desired_generation, Some(9));

    // Another deploy's hand-off cannot start over on this state file.
    let mut other = handoff(true);
    other.commit = CommitId::parse(&"ee".repeat(20)).unwrap();
    std::fs::write(&f.handoff, other.to_bytes().unwrap()).unwrap();
    let (code, _, err) = exec(&run_args(&f, &metrics, &waive));
    assert_eq!(code, EXIT_USAGE);
    assert!(err.contains("the hand-off names another"), "{err}");
}

/// A hand-off that is not understood is a usage error, and nothing is written.
#[test]
fn a_bad_handoff_is_refused() {
    let f = files("handoff-bad", true);
    std::fs::write(&f.handoff, "{}").unwrap();
    let (metrics, _) = api("200 OK", &Farm::healthy(0.0).text());
    let (code, _, err) = exec(&run_args(&f, &metrics, &[]));
    assert_eq!(code, EXIT_USAGE);
    assert!(err.contains("hand-off"), "{err}");
    assert!(!f.state.exists() && !f.verdict.exists());
}

fn sample_args(farm: &str, ledger: &Path) -> Vec<String> {
    [
        "kbf-bake",
        "sample",
        "--metrics-url",
        &format!("{farm}/metrics"),
        "--ledger",
        &s(ledger),
    ]
    .map(String::from)
    .to_vec()
}

/// Catches `sample` writing nothing, or a farm without the histogram being recorded as a reading:
/// a scrape with the histogram adds one reading with its buckets; one without fails and leaves the
/// ledger as it was.
#[test]
fn sample_adds_a_reading_or_fails_and_leaves_the_ledger() {
    let dir = scratch("sample");
    let ledger = dir.join("ledger.json");
    let (farm, _) = api("200 OK", &Farm::healthy(5.0).text());
    let (code, out, err) = exec(&sample_args(&farm, &ledger));
    assert_eq!(code, EXIT_GREEN, "{err}");
    assert!(out.contains("1 readings"), "{out}");
    let l = Ledger::load(&ledger).unwrap();
    assert_eq!(l.snapshots.len(), 1);
    assert_eq!(l.snapshots[0].buckets.len(), 2);

    let (bare, _) = api(
        "200 OK",
        &Farm::healthy(5.0)
            .without("kbf_execute_queued_seconds")
            .text(),
    );
    let (code, _, err) = exec(&sample_args(&bare, &ledger));
    assert_eq!(code, EXIT_USAGE);
    assert!(err.contains("kbf_execute_queued_seconds"), "{err}");
    assert_eq!(Ledger::load(&ledger).unwrap(), l);
}

const AT: u64 = 1_800_000_000;

fn full_ledger(path: &Path) {
    let mut l = Ledger::default();
    let mut t = AT - BASELINE_SECS;
    let mut n = 0.0;
    while t <= AT {
        l.push(t, &[(0.5, n), (f64::INFINITY, n)]).unwrap();
        n += 100.0;
        t += 3600;
    }
    l.save(path).unwrap();
}

fn baseline_args(f: &Files, ledger: &Path, farm: &str, out: &Path) -> Vec<String> {
    [
        "kbf-bake",
        "baseline",
        "--ledger",
        &s(ledger),
        "--invocations-url",
        farm,
        "--out",
        &s(out),
    ]
    .map(String::from)
    .to_vec()
    .into_iter()
    .chain(["--handoff".to_owned(), s(&f.handoff)])
    .collect()
}

/// Catches `baseline` reading the wrong moment or writing a half baseline as complete: with the
/// hand-off's deploy start it asks for the seven days before it, writes both values and exits 0;
/// with a farm that does not answer a list it still writes the file, with the reason and no error
/// ratio, prints it, and exits 4.
#[test]
fn the_baseline_verb_takes_both_values_at_the_deploy_start_or_names_what_is_missing() {
    let f = files("baseline-verb", true);
    let mut h = handoff(true);
    h.deploy_start = AT;
    h.deploy_end = None;
    std::fs::write(&f.handoff, h.to_bytes().unwrap()).unwrap();
    let ledger = f.dir.join("ledger.json");
    full_ledger(&ledger);
    let out_path = f.dir.join("out-baseline.json");

    let list = r#"{"invocations":[{"counts":{"ran":200,"hits":0,"failed":10}}]}"#;
    let (farm, rx) = api("200 OK", list);
    let (code, _, err) = exec(&baseline_args(&f, &ledger, &farm, &out_path));
    assert_eq!(code, EXIT_GREEN, "{err}");
    let req = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        req.contains("since=2027-01-08T08:00:00Z&until=2027-01-15T08:00:00Z"),
        "{req}"
    );
    let doc = BaselineDoc::load(&out_path).unwrap();
    assert_eq!(
        (doc.error_ratio, doc.queue_p99, doc.taken_at),
        (Some(0.05), Some(0.5), AT)
    );

    let (farm, _) = api("200 OK", "{}");
    let (code, _, err) = exec(&baseline_args(&f, &ledger, &farm, &out_path));
    assert_eq!(code, EXIT_NO_BASELINE);
    assert!(err.contains("no baseline: error ratio"), "{err}");
    let doc = BaselineDoc::load(&out_path).unwrap();
    assert_eq!((doc.error_ratio, doc.queue_p99), (None, Some(0.5)));
    assert_eq!(doc.missing.len(), 1);
}

/// Catches a rollback whose record is lost without a word: the state file cannot be written after
/// the offer (here the file is swapped for a directory while the API answers), and the run says so
/// on standard error and still ends red with the offer made.
#[test]
fn a_rollback_that_cannot_be_recorded_says_so() {
    let f = files("rb-unrecorded", true);
    let state = f.state.clone();
    let (url, rx) = api_with("200 OK", "{}", move || {
        let _ = std::fs::remove_file(&state);
        let _ = std::fs::create_dir(&state);
    });
    let (code, out, err) = run_with_rollback(&f, &broken(), &url, &[]);
    assert_eq!(code, EXIT_RED);
    assert!(out.contains("ROLLBACK: offered"), "{out}");
    assert!(
        err.contains("the rollback was not recorded in the state"),
        "{err}"
    );
    assert!(rx.recv_timeout(Duration::from_secs(5)).is_ok());
}

/// A baseline file that is not one is a usage error before anything is written.
#[test]
fn a_bad_baseline_file_is_refused() {
    let f = files("baseline-bad-file", true);
    std::fs::write(&f.baseline, "[]").unwrap();
    let (metrics, _) = api("200 OK", &Farm::healthy(0.0).text());
    let (code, _, err) = exec(&run_args(&f, &metrics, &[]));
    assert_eq!(code, EXIT_USAGE);
    assert!(err.contains("is not a baseline"), "{err}");
    assert!(!f.state.exists() && !f.verdict.exists());
}

/// Catches `--at` ignored and the default not being now: with `--at` the request names the seven
/// days before it; with neither `--at` nor a hand-off the moment is now, and a ledger that does not
/// reach back from now is no baseline (exit 4, the file says why).
#[test]
fn the_baseline_moment_is_at_or_now() {
    let f = files("baseline-at", true);
    let ledger = f.dir.join("ledger.json");
    full_ledger(&ledger);
    let out_path = f.dir.join("b.json");
    let list = r#"{"invocations":[{"counts":{"ran":10,"hits":0,"failed":1}}]}"#;
    let args = |farm: &str, tail: &[&str]| -> Vec<String> {
        let mut a: Vec<String> = [
            "kbf-bake",
            "baseline",
            "--ledger",
            &s(&ledger),
            "--invocations-url",
            farm,
            "--out",
            &s(&out_path),
        ]
        .map(String::from)
        .to_vec();
        a.extend(tail.iter().map(|x| (*x).to_owned()));
        a
    };
    let (farm, rx) = api("200 OK", list);
    let (code, _, err) = exec(&args(&farm, &["--at", &AT.to_string()]));
    assert_eq!(code, EXIT_GREEN, "{err}");
    let req = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(req.contains("since=2027-01-08T08:00:00Z"), "{req}");

    let (farm, rx) = api("200 OK", list);
    let (code, _, err) = exec(&args(&farm, &[]));
    assert_eq!(code, EXIT_NO_BASELINE);
    assert!(err.contains("queue p99"), "{err}");
    let req = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        !req.contains("since=2027-01-08"),
        "now is not the fixed moment: {req}"
    );
}
