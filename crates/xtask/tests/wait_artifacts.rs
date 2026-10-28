//! `cargo xtask wait-artifacts` as the coverage run of `ci.yml` calls it: the real binary,
//! with `xtask-fake` as `gh` (`gh-api.json` holds what GitHub answers to a GET).

use std::path::{Path, PathBuf};
use std::process::Command;

use kbf_testkit::dir::TestDir;
use serde_json::{Value, json};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const JOBS: &str = "repos/o/r/actions/runs/7/jobs?per_page=100";
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
        let out = Command::new(XTASK)
            .args(["wait-artifacts", "--repo", "o/r", "--run-id", "7"])
            .args([
                "--artifact-prefix",
                "profraw-",
                "--job-prefix",
                "test-shard",
            ])
            .args(["--of", "3", "--poll-seconds", "0", "--polls", polls, "--gh"])
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
    json!({ "jobs": [{ "name": "test-shard (3)", "conclusion": null }] })
}

fn made(names: &[&str]) -> Value {
    json!({ "artifacts": names.iter().map(|n| json!({ "name": n })).collect::<Vec<_>>() })
}

/// Catches: the verb not asking GitHub through `gh api`, or not printing that it is done.
#[test]
fn returns_when_the_other_parts_have_uploaded() {
    let f = fx(
        "ready",
        &json!({ JOBS: running(), ARTIFACTS: made(&["profraw-1", "profraw-2"]) }),
    );
    let (code, out, err) = f.run("3");
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(
        out,
        "wait-artifacts: 2 artifact(s) named profraw-* are there (wanted 2)\n"
    );
}

/// Catches: looks that never end when an artifact does not come (the real pause between
/// looks runs here, at no seconds), and a failed part not ending the wait.
#[test]
fn gives_up_and_stops_for_a_failed_part() {
    let f = fx(
        "short",
        &json!({ JOBS: running(), ARTIFACTS: made(&["profraw-1"]) }),
    );
    let (code, _, err) = f.run("2");
    assert_eq!(code, Some(1));
    assert!(
        err.contains("after 2 look(s) at the run only 1 of 2 artifact(s)"),
        "{err}"
    );

    let failed = json!({ "jobs": [{ "name": "test-shard (1)", "conclusion": "failure" }] });
    let f = fx("failed", &json!({ JOBS: failed, ARTIFACTS: made(&[]) }));
    let (code, _, err) = f.run("2");
    assert_eq!(code, Some(1));
    assert!(
        err.contains("test-shard (1) (failure) did not succeed"),
        "{err}"
    );
}

/// Catches: a run GitHub does not know (HTTP 404) read as no artifacts yet, which would
/// wait out every look.
#[test]
fn a_missing_run_fails_at_once() {
    let f = fx("missing", &json!({}));
    let (code, _, err) = f.run("5");
    assert_eq!(code, Some(1));
    assert!(err.contains("not found"), "{err}");
}
