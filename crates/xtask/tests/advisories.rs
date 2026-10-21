//! `cargo xtask advisories` as the advisories workflow runs it: the real binary, with
//! `xtask-fake` as `cargo deny` (it prints `deny-stderr`) and as `gh` (`gh-api.json` holds
//! what GitHub would answer to a GET, `gh-calls` what each write sent: the arguments and
//! the body `gh` would have read from stdin).

use std::path::{Path, PathBuf};
use std::process::Command;

use kbf_testkit::dir::TestDir;
use serde_json::{Value, json};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const REPO: &str = "komira-ai/kbf";
const SHA: &str = "357494ac6b0fecfa065d6c8fa038cfca9ae9c13f";
const TITLE: &str = "Security advisories in kbf's dependencies";
const LIST: &str = "repos/komira-ai/kbf/issues?state=open&per_page=100&page=1";
const LABEL: &str = "repos/komira-ai/kbf/labels/security";

struct Fx {
    root: PathBuf,
    /// Last field: the directory is removed with the fixture.
    _own: TestDir,
}

fn fx(name: &str) -> Fx {
    let own = TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-advisories"),
        name,
    );
    let root = own.to_path_buf();
    for program in ["gh", "cargo"] {
        std::fs::create_dir_all(root.join(program)).expect("dir");
        std::fs::hard_link(FAKE, root.join(program).join(program)).expect("link the fake");
    }
    Fx { root, _own: own }
}

fn advisory_line() -> String {
    json!({
        "type": "diagnostic",
        "fields": {
            "code": "vulnerability", "severity": "error", "message": "Out of bounds",
            "graphs": [{ "Krate": { "name": "tokio", "version": "1.2.3" } }],
            "advisory": {
                "id": "RUSTSEC-2026-0001", "package": "tokio", "title": "Out of bounds",
                "url": "https://rustsec.org/advisories/RUSTSEC-2026-0001",
            },
        }
    })
    .to_string()
}

impl Fx {
    fn put(&self, name: &str, text: &str) {
        std::fs::write(self.root.join(name), text).expect("write");
    }

    /// What cargo-deny prints and how it exits, and what GitHub answers.
    fn script(&self, deny: &[String], exit: u8, api: Value) {
        self.put("deny-stderr", &(deny.join("\n") + "\n"));
        self.put("deny-exit", &exit.to_string());
        self.put("gh/gh-api.json", &api.to_string());
    }

    fn run(&self, extra: &[&str]) -> (Option<i32>, String, String) {
        let out = Command::new(XTASK)
            .arg("--root")
            .arg(&self.root)
            .args(["advisories", "--repo", REPO, "--gh"])
            .arg(self.root.join("gh/gh"))
            .args(extra)
            .env("CARGO", self.root.join("cargo/cargo"))
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

// Catches: cargo-deny run with other arguments than ci.yml's checks (all checks, no JSON,
// no manifest), or from another directory.
#[test]
fn cargo_deny_runs_the_advisories_check_in_json() {
    let f = fx("deny-args");
    f.script(&[], 0, json!({ LIST: [] }));
    let (code, _, err) = f.run(&["--commit", SHA]);
    assert_eq!(code, Some(0), "{err}");
    let args = std::fs::read_to_string(f.root.join("target/debug/cargo-args")).expect("args");
    assert_eq!(
        args,
        "deny --log-level warn --format json --manifest-path ./Cargo.toml --all-features check \
         advisories\n"
    );
}

// Catches: the body of a create going to `gh` without `--input -` (gh ignores it, the
// issue has no title), or an exit status other than 0 for a run that found advisories.
#[test]
fn advisories_open_an_issue_through_gh_with_input_dash_and_exit_zero() {
    let f = fx("create");
    f.script(
        &[advisory_line()],
        1,
        json!({ LIST: [], LABEL: { "name": "security" } }),
    );
    let (code, out, err) = f.run(&["--commit", SHA]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(
        out,
        "1 advisories: opened https://github.com/komira-ai/kbf/issues/42\n"
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
    assert_eq!(create["body"]["title"], TITLE);
    assert_eq!(create["body"]["labels"], json!(["security"]));
    let text = create["body"]["body"].as_str().expect("text");
    assert!(
        text.contains("| RUSTSEC-2026-0001 | tokio | 1.2.3 |"),
        "{text}"
    );
    assert!(text.contains(SHA), "{text}");
}

// Catches: an update that PATCHes without a body, or an issue number lost on the way.
#[test]
fn advisories_update_the_open_issue() {
    let f = fx("update");
    f.script(
        &[advisory_line()],
        1,
        json!({ LIST: [{ "number": 7, "title": TITLE }] }),
    );
    let (code, out, err) = f.run(&["--commit", SHA]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, "1 advisories: updated issue #7\n");
    let calls = f.calls();
    let [update] = &calls[..] else {
        panic!("one write: {calls:?}")
    };
    assert_eq!(
        update["args"],
        json!([
            "api",
            "-X",
            "PATCH",
            "repos/komira-ai/kbf/issues/7",
            "--input",
            "-"
        ])
    );
    assert!(
        update["body"]["body"]
            .as_str()
            .expect("text")
            .contains("tokio")
    );
}

// Catches: no commit when `--commit` is not given (the workflow passes none), a comment
// without a body, and a close that is not sent.
#[test]
fn no_advisories_close_the_issue_at_head_of_the_checkout() {
    let f = fx("close");
    f.script(&[], 0, json!({ LIST: [{ "number": 7, "title": TITLE }] }));
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&f.root)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    git(&["init", "-q"]);
    git(&["commit", "-q", "--allow-empty", "-m", "base"]);
    let head = git(&["rev-parse", "HEAD"]);
    let (code, out, err) = f.run(&[]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, "no advisories: closed issue #7\n");
    let calls = f.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(
        calls[0]["body"]["body"],
        format!("No advisories as of {head}")
    );
    assert_eq!(calls[1]["body"]["state"], "closed");
}

// Catches: a run with nothing to report that writes anything.
#[test]
fn no_advisories_and_no_issue_make_no_call() {
    let f = fx("nothing");
    f.script(&[], 0, json!({ LIST: [] }));
    let (code, out, err) = f.run(&["--commit", SHA]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, "no advisories, and no open issue\n");
    assert!(f.calls().is_empty());
}

// Catches: a tool failure that exits 0 (the workflow would be green with no check made),
// or one that still writes to the issue.
#[test]
fn a_failed_tool_exits_one_and_writes_nothing() {
    // cargo-deny failed on its own: an ERROR log.
    let f = fx("deny-fails");
    let error = json!({ "type": "log", "fields": { "level": "ERROR", "message": "no database" } });
    f.script(
        &[error.to_string()],
        1,
        json!({ LIST: [{ "number": 7, "title": TITLE }] }),
    );
    let (code, _, err) = f.run(&["--commit", SHA]);
    assert_eq!(code, Some(1));
    assert!(err.contains("no database"), "{err}");
    assert!(f.calls().is_empty());
    // gh failed: the list is not found.
    let f = fx("gh-fails");
    f.script(&[], 0, json!({}));
    let (code, _, err) = f.run(&["--commit", SHA]);
    assert_eq!(code, Some(1));
    assert!(err.contains("not found"), "{err}");
    // A program that does not exist.
    let out = Command::new(XTASK)
        .arg("--root")
        .arg(&f.root)
        .args([
            "advisories",
            "--repo",
            REPO,
            "--commit",
            SHA,
            "--gh",
            "/nonexistent/gh",
        ])
        .env("CARGO", f.root.join("cargo/cargo"))
        .output()
        .expect("run xtask");
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("/nonexistent/gh"));
    let out = Command::new(XTASK)
        .arg("--root")
        .arg(&f.root)
        .args(["advisories", "--repo", REPO, "--commit", SHA])
        .env("CARGO", "/nonexistent/cargo")
        .output()
        .expect("run xtask");
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cargo deny"));
}

// Catches: a repository or commit that is not one reaching a URL or a comment (exit 2
// is a usage error, as for every verb).
#[test]
fn a_bad_repo_or_commit_is_a_usage_error() {
    let f = fx("usage");
    f.script(&[], 0, json!({ LIST: [] }));
    let out = Command::new(XTASK)
        .arg("--root")
        .arg(&f.root)
        .args(["advisories", "--repo", "kbf"])
        .output()
        .expect("run xtask");
    assert_eq!(out.status.code(), Some(2));
    let (code, _, err) = f.run(&["--commit", "main"]);
    assert_eq!(code, Some(2), "{err}");
}

// Catches: a failure of `gh` that is not a 404, a reply that is not JSON, or a write
// answered with nothing, read as "no issue" or as a crash (an empty reply is a success).
#[test]
fn gh_failing_or_answering_oddly_is_told_apart() {
    for (defect, want) in [("api-fails", "HTTP 502"), ("api-garbage", "not JSON")] {
        let f = fx(defect);
        f.script(&[], 0, json!({ LIST: [] }));
        let out = Command::new(XTASK)
            .arg("--root")
            .arg(&f.root)
            .args(["advisories", "--repo", REPO, "--commit", SHA, "--gh"])
            .arg(f.root.join("gh/gh"))
            .env("CARGO", f.root.join("cargo/cargo"))
            .env("XTASK_FAKE_DEFECT", defect)
            .output()
            .expect("run xtask");
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{err}");
        assert!(err.contains(want), "{err}");
    }
    let f = fx("empty-reply");
    f.script(&[], 0, json!({ LIST: [{ "number": 7, "title": TITLE }] }));
    f.put("gh/gh-fault", "empty-reply");
    let (code, out, err) = f.run(&["--commit", SHA]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, "no advisories: closed issue #7\n");
    assert_eq!(f.calls().len(), 2);
}

// Catches: a write that GitHub answered with 404 (a token that cannot write issues, a
// repository that moved) read as "not found" and so as success, as a GET's 404 is; and a
// checkout that is not a repository (no `--commit`, no `HEAD`) read as an empty commit.
#[test]
fn a_refused_write_and_a_missing_head_are_failures() {
    let f = fx("write-404");
    f.script(&[], 0, json!({ LIST: [{ "number": 7, "title": TITLE }] }));
    f.put("gh/gh-fault", "fail-write");
    let (code, _, err) = f.run(&["--commit", SHA]);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("HTTP 404"), "{err}");
    // No `--commit`, and a root that no git repository holds.
    let f = fx("no-head");
    f.script(&[], 0, json!({ LIST: [] }));
    let out = Command::new(XTASK)
        .arg("--root")
        .arg(&f.root)
        .args(["advisories", "--repo", REPO, "--gh"])
        .arg(f.root.join("gh/gh"))
        .env("CARGO", f.root.join("cargo/cargo"))
        .env("GIT_CEILING_DIRECTORIES", f.root.parent().expect("parent"))
        .output()
        .expect("run xtask");
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("git rev-parse"));
}
