//! `cargo xtask wait-artifacts` as the `test` job of `ci.yml` calls it: the real binary,
//! with `xtask-fake` as `gh` (`gh-api.json` holds what GitHub answers to a GET).

use std::path::{Path, PathBuf};
use std::process::Command;

use kbf_testkit::dir::TestDir;
use serde_json::{Value, json};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const RUN: &str = "repos/o/r/actions/runs/7";
const JOBS: &str = "repos/o/r/actions/runs/7/jobs?filter=latest&per_page=100";
const ARTIFACTS: &str = "repos/o/r/actions/runs/7/artifacts?per_page=100";

struct Fx {
    root: PathBuf,
    /// Last field: the directory is removed with the fixture.
    _own: TestDir,
}

fn fx(name: &str, api: &Value) -> Fx {
    let own = TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-wait-artifacts"),
        name,
    );
    let root = own.to_path_buf();
    std::fs::create_dir_all(root.join("gh")).expect("dir");
    std::fs::hard_link(FAKE, root.join("gh/gh")).expect("link the fake");
    std::fs::write(root.join("gh/gh-api.json"), api.to_string()).expect("answers");
    Fx { root, _own: own }
}

impl Fx {
    fn run(&self, polls: &str) -> (Option<i32>, String, String) {
        self.run_for(polls, &["profraw-legacy-1", "profraw-js-1"])
    }

    fn run_for(&self, polls: &str, artifacts: &[&str]) -> (Option<i32>, String, String) {
        let mut command = Command::new(XTASK);
        command.args(["wait-artifacts", "--repo", "o/r", "--run-id", "7"]);
        for a in artifacts {
            command.args(["--artifact", a]);
        }
        let out = command
            .args(["--job-prefix", "test-shard"])
            .args(["--poll-seconds", "0", "--polls", polls, "--gh"])
            .arg(self.root.join("gh/gh"))
            .current_dir(&self.root)
            .output()
            .expect("run xtask");
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
}

fn running() -> Value {
    json!({ "jobs": [{ "name": "test-shard (js-1-of-1)", "conclusion": null }] })
}

fn made(names: &[&str]) -> Value {
    json!({ "artifacts": names.iter().map(|n| json!({ "name": n })).collect::<Vec<_>>() })
}

/// Catches: the verb not asking GitHub through `gh api`, or not printing that it is done.
#[test]
fn returns_when_the_named_artifacts_have_been_uploaded() {
    let f = fx(
        "ready",
        &json!({ RUN: json!({}), JOBS: running(), ARTIFACTS: made(&["profraw-legacy-1", "profraw-js-1"]) }),
    );
    let (code, out, err) = f.run("3");
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(
        out,
        "wait-artifacts: the 2 artifact(s) are there: profraw-legacy-1, profraw-js-1\n"
    );
}

/// Catches: looks that never end when an artifact does not come (the real pause between
/// looks runs here, at no seconds), and a failed shard not ending the wait.
#[test]
fn gives_up_and_stops_for_a_failed_shard() {
    let f = fx(
        "short",
        &json!({ RUN: json!({}), JOBS: running(), ARTIFACTS: made(&["profraw-js-1"]) }),
    );
    let (code, _, err) = f.run("2");
    assert_eq!(code, Some(1));
    assert!(
        err.contains(
            "after 2 look(s) at the run 1 of 2 artifact(s) have not come: profraw-legacy-1"
        ),
        "{err}"
    );

    let failed =
        json!({ "jobs": [{ "name": "test-shard (legacy-1-of-3)", "conclusion": "failure" }] });
    let f = fx(
        "failed",
        &json!({ RUN: json!({}), JOBS: failed, ARTIFACTS: made(&[]) }),
    );
    let (code, _, err) = f.run("2");
    assert_eq!(code, Some(1));
    assert!(
        err.contains("test-shard (legacy-1-of-3) (failure) did not succeed"),
        "{err}"
    );
}

/// Catches: a run GitHub does not know (HTTP 404) read as no artifacts yet, which would
/// wait out every look; it is asked about again until `FAILED_LOOKS` looks in a row have
/// failed, and then the wait fails with the error. The looks are more than that, so the count
/// ends the wait and not the end of the looks.
#[test]
fn a_missing_run_fails_after_the_failed_looks_in_a_row() {
    let f = fx("missing", &json!({}));
    let (code, _, err) = f.run("40");
    assert_eq!(code, Some(1));
    assert!(err.contains("not found"), "{err}");
}

/// Catches: the verb run with no `--artifact`, which would return at once having waited for
/// nothing.
#[test]
fn it_needs_an_artifact_to_wait_for() {
    let f = fx(
        "none",
        &json!({ RUN: json!({}), JOBS: running(), ARTIFACTS: made(&[]) }),
    );
    let out = Command::new(XTASK)
        .args(["wait-artifacts", "--repo", "o/r", "--run-id", "7"])
        .args(["--job-prefix", "test-shard", "--gh"])
        .arg(f.root.join("gh/gh"))
        .current_dir(&f.root)
        .output()
        .expect("run xtask");
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--artifact"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

const TEAM: [&str; 2] = [
    "profraw-legacy-1=(legacy-1-of-1)",
    "profraw-js-1=(js-1-of-1)",
];

fn dated(name: &str, created: &str) -> Value {
    json!({ "name": name, "created_at": created })
}

fn job(name: &str, conclusion: Value, started: &str) -> Value {
    json!({ "name": name, "conclusion": conclusion, "run_attempt": 2, "started_at": started })
}

/// Catches: the real verb not telling a rerun of every job from a rerun of the failed one.
/// After "Re-run all jobs" the artifacts are older than the new jobs' starts and are not counted
/// while the shards run again; after "Re-run failed jobs" the shard that passed comes back with
/// its original start and keeps its artifact, and the one that runs again is waited for.
#[test]
fn a_rerun_counts_the_artifacts_of_the_shards_that_ran_in_it() {
    let old = "2026-10-01T10:00:00Z";
    let start = "2026-10-01T11:00:00Z";
    let new = "2026-10-01T11:30:00Z";
    let everything = json!({ "jobs": [
        job("test-shard (legacy-1-of-1)", Value::Null, start),
        job("test-shard (js-1-of-1)", Value::Null, start),
    ]});
    let f = fx(
        "rerun-all",
        &json!({ RUN: json!({ "run_attempt": 2, "run_started_at": start }), JOBS: everything, ARTIFACTS: { "artifacts": [
            dated("profraw-legacy-1", old), dated("profraw-js-1", old)
        ]}}),
    );
    let (code, _, err) = f.run_for("2", &TEAM);
    assert_eq!(code, Some(1), "{err}");
    assert!(
        err.contains("2 of 2 artifact(s) have not come: profraw-legacy-1, profraw-js-1"),
        "{err}"
    );

    let failed_only = json!({ "jobs": [
        job("test-shard (legacy-1-of-1)", json!("success"), old),
        job("test-shard (js-1-of-1)", Value::Null, start),
    ]});
    let f = fx(
        "rerun-failed",
        &json!({ RUN: json!({ "run_attempt": 2, "run_started_at": start }), JOBS: failed_only, ARTIFACTS: { "artifacts": [
            dated("profraw-legacy-1", old), dated("profraw-js-1", new)
        ]}}),
    );
    let (code, out, err) = f.run_for("2", &TEAM);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(
        out,
        "wait-artifacts: the 2 artifact(s) are there: profraw-legacy-1, profraw-js-1\n"
    );
}
