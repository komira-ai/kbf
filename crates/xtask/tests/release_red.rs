//! `cargo xtask release-red` as the suites workflow runs it: the real binary, with
//! `xtask-fake` as `gh` (`gh-api.json` holds what GitHub would answer to a GET, `gh-calls`
//! what each write sent).

use std::path::{Path, PathBuf};
use std::process::Command;

use kbf_testkit::dir::TestDir;
use serde_json::{Value, json};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const REPO: &str = "komira-ai/kbf";
const HEAD: &str = "357494ac6b0fecfa065d6c8fa038cfca9ae9c13f";
const URL: &str = "https://github.com/komira-ai/kbf/actions/runs/77";
const JOBS: &str = "repos/komira-ai/kbf/actions/runs/77/jobs?per_page=100&filter=latest&page=1";
const ISSUES: &str = "repos/komira-ai/kbf/issues?state=open&labels=main-red&per_page=100&page=1";
const RUNS: &str = "repos/komira-ai/kbf/actions/workflows/5/runs?branch=main&event=push&status=success&per_page=20";

struct Fx {
    root: PathBuf,
    /// Last field: the directory is removed with the fixture.
    _own: TestDir,
}

fn fx(name: &str, api: Value) -> Fx {
    let own = TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-release-red"),
        name,
    );
    let root = own.to_path_buf();
    std::fs::create_dir_all(root.join("gh")).expect("dir");
    std::fs::hard_link(FAKE, root.join("gh/gh")).expect("link the fake");
    std::fs::write(root.join("gh/gh-api.json"), api.to_string()).expect("write");
    Fx { root, _own: own }
}

fn api(job: &str, conclusion: &str, issues: Value) -> Value {
    json!({
        "repos/komira-ai/kbf/actions/runs/77": {
            "html_url": URL, "head_sha": HEAD, "head_branch": "main", "event": "push",
            "workflow_id": 5, "name": "suites", "created_at": "2026-11-02T10:00:00Z",
        },
        JOBS: { "jobs": [{ "id": 11, "name": job, "status": "completed", "conclusion": conclusion }] },
        ISSUES: issues,
        RUNS: { "workflow_runs": [] },
        "repos/komira-ai/kbf/actions/workflows/5/runs?branch=main&event=push&per_page=1":
            { "workflow_runs": [{ "id": 77 }] },
        "repos/komira-ai/kbf/check-runs/11/annotations": [],
        "repos/komira-ai/kbf/labels/main-red": { "name": "main-red" },
    })
}

impl Fx {
    fn run(&self) -> (Option<i32>, String, String) {
        let out = Command::new(XTASK)
            .arg("--root")
            .arg(&self.root)
            .args(["release-red", "--repo", REPO, "--run-id", "77", "--gh"])
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

    fn calls(&self) -> Vec<Value> {
        let text = std::fs::read_to_string(self.root.join("gh/gh-calls")).unwrap_or_default();
        text.lines()
            .map(|l| serde_json::from_str(l).expect("a call"))
            .collect()
    }
}

// Catches: the issue body going to `gh` without `--input -` (the issue would have no
// title), or a red run that exits non-zero.
#[test]
fn a_red_run_opens_the_issue_through_gh_and_exits_zero() {
    let f = fx("red", api("beta / x", "failure", json!([])));
    let (code, out, err) = f.run();
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(
        out,
        "red: opened https://github.com/komira-ai/kbf/issues/42\n"
    );
    let calls = f.calls();
    let [create] = &calls[..] else {
        panic!("one write: {calls:?}")
    };
    assert_eq!(
        create["args"],
        json!([
            "api",
            "-X",
            "POST",
            "repos/komira-ai/kbf/issues",
            "--input",
            "-"
        ])
    );
    assert_eq!(create["body"]["title"], "RELEASE-RED: kbf beta / x");
    assert_eq!(create["body"]["labels"], json!(["main-red"]));
    assert_eq!(
        create["body"]["body"],
        format!("Run: {URL}\nHead: {HEAD}\nLast green: none\nCulprits: unknown\nFailing: beta / x")
    );
}

// Catches: a comment without a body, a close that is not sent, a close before the comment.
#[test]
fn a_green_run_comments_and_closes_the_open_issue() {
    let issues = json!([{ "number": 7, "title": "RELEASE-RED: kbf beta / x" }]);
    let f = fx("green", api("beta / x", "success", issues));
    let (code, out, err) = f.run();
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, "green: closed issue #7\n");
    let calls = f.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["args"][3], "repos/komira-ai/kbf/issues/7/comments");
    assert_eq!(
        calls[0]["body"]["body"],
        format!("Fixed: suites is green at {HEAD} ({URL})")
    );
    assert_eq!(calls[1]["args"][2], "PATCH");
    assert_eq!(calls[1]["body"]["state"], "closed");
}

// Catches: a refused call reported as success.
#[test]
fn a_refused_write_exits_one() {
    let f = fx("refused", api("beta / x", "failure", json!([])));
    std::fs::write(f.root.join("gh/gh-fault"), "fail-write").expect("fault");
    let (code, _, err) = f.run();
    assert_eq!(code, Some(1), "{err}");
}
