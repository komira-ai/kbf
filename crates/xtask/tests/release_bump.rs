//! `cargo xtask release bump-internal` against a recorded world. The seam `Tools` is
//! answered from GitHub's answers by path, the pull requests and the gitlink the clone
//! holds, and every `gh` and `git` call is written down. The real runner and the binary are
//! tested in `release_bump_runner.rs`.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::{Value, json};
use xtask::Error;
use xtask::release::bump::{Bump, BumpArgs, Ran, Tools, apply};

const REPO: &str = "komira-ai/kbf";
const INTERNAL: &str = "example-org/internal";
/// The commit the internal repository's submodule is at.
const OLD: &str = "1111111111111111111111111111111111111111";
/// The released commit.
const NEW: &str = "2222222222222222222222222222222222222222";
/// Another commit.
const OTHER: &str = "3333333333333333333333333333333333333333";
/// Where the fake clone is: the fake git never touches the disk.
const DIR: &str = "/work/clone";
const BRANCH: &str = "bump/kbf-v1.2.3";
const TITLE: &str = "chore(kbf): bump to v1.2.3";
const PR_URL: &str = "https://github.com/example-org/internal/pull/7";

/// What a call that changes something on GitHub or in the clone holds.
const CHANGES: [&str; 6] = [
    "update-index",
    "commit --quiet",
    "push --quiet",
    "gh pr create",
    "gh pr merge",
    "gh pr close",
];

fn printed(stdout: &str) -> Ran {
    Ran {
        ok: true,
        stdout: stdout.to_owned(),
        stderr: String::new(),
    }
}

fn broke(stderr: &str) -> Ran {
    Ran {
        ok: false,
        stdout: String::new(),
        stderr: stderr.to_owned(),
    }
}

/// The world one test runs in: what GitHub and the clone answer, and every call made.
struct World {
    /// GitHub's answers to `gh api <path>`; a path not here is a 404.
    api: BTreeMap<String, Value>,
    /// What the search `gh pr list --search` prints.
    prs: String,
    /// What the list of one head branch, `gh pr list --head`, prints.
    own: String,
    /// What `git ls-tree` prints instead of the gitlink at HEAD, when set.
    tree: Option<String>,
    /// What `gh pr create` prints.
    created: String,
    /// Whether `gh pr view` says auto-merge is on once `gh pr merge` ran.
    auto_merge_takes: bool,
    /// What `gh pr view` prints instead, when set.
    view: Option<String>,
    /// A `gh api` call whose path holds this text prints something that is not JSON.
    garbage: Option<&'static str>,
    /// What a `gh api` call for a path not in `api` prints on stderr.
    missing: &'static str,
    /// The gitlink at HEAD of the clone.
    head: RefCell<String>,
    /// Whether a commit moves HEAD's gitlink to the staged one.
    commit_moves_head: bool,
    /// What `git diff --cached` prints instead of the one-gitlink diff, when set.
    diff: Option<String>,
    /// A call whose line holds the first text fails, printing the second on stderr.
    fail: Option<(&'static str, &'static str)>,
    /// How many calls that would fail pass first.
    fail_after: usize,
    /// How many calls the failure matched so far.
    matched: Cell<usize>,
    /// A call whose line holds this text cannot be started.
    spawn_fail: Option<&'static str>,
    /// The gitlink `git update-index` staged.
    staged: RefCell<Option<String>>,
    /// Whether `gh pr merge` ran.
    merged: Cell<bool>,
    /// Every call: the tool, then its arguments joined by spaces.
    calls: RefCell<Vec<String>>,
}

fn tag_path() -> String {
    format!("repos/{REPO}/commits/refs/tags/v1.2.3")
}

fn compare_path(old: &str) -> String {
    format!("repos/{REPO}/compare/{old}...{NEW}")
}

fn ref_path() -> String {
    format!("repos/{INTERNAL}/git/ref/heads/{BRANCH}")
}

fn contents_path() -> String {
    format!("repos/{INTERNAL}/contents/kbf?ref={BRANCH}")
}

/// The good world: the tag names the released commit, the submodule is at `OLD`, which the
/// released commit is ahead of, no bump pull request or branch exists, and everything works.
fn world() -> World {
    let mut w = World {
        api: BTreeMap::new(),
        prs: "[]".to_owned(),
        own: "[]".to_owned(),
        tree: None,
        created: format!("Creating pull request\n\n{PR_URL}\n"),
        auto_merge_takes: true,
        view: None,
        garbage: None,
        missing: "gh: Not Found (HTTP 404)",
        head: RefCell::new(OLD.to_owned()),
        commit_moves_head: true,
        diff: None,
        fail: None,
        fail_after: 0,
        matched: Cell::new(0),
        spawn_fail: None,
        staged: RefCell::new(None),
        merged: Cell::new(false),
        calls: RefCell::new(Vec::new()),
    };
    w.set(&tag_path(), json!({ "sha": NEW }));
    w.set(&compare_path(OLD), json!({ "status": "ahead" }));
    w
}

impl World {
    fn set(&mut self, path: &str, answer: Value) {
        self.api.insert(path.to_owned(), answer);
    }

    fn record(&self, tool: &str, args: &[&str]) -> String {
        let line = format!("{tool} {}", args.join(" "));
        self.calls.borrow_mut().push(line.clone());
        line
    }

    fn broken(&self, line: &str) -> Option<Ran> {
        let (needle, why) = self.fail?;
        if !line.contains(needle) {
            return None;
        }
        let seen = self.matched.get();
        self.matched.set(seen + 1);
        (seen >= self.fail_after).then(|| broke(why))
    }

    fn unstartable(&self, line: &str) -> Option<Error> {
        let needle = self.spawn_fail?;
        if !line.contains(needle) {
            return None;
        }
        let what = "the program".to_owned();
        Some(Error::Spawn {
            what,
            why: "no such file".to_owned(),
        })
    }

    fn api_answer(&self, path: &str) -> Ran {
        if self.garbage.is_some_and(|g| path.contains(g)) {
            return printed("<html>");
        }
        match self.api.get(path) {
            Some(answer) => printed(&answer.to_string()),
            None => broke(self.missing),
        }
    }

    fn view_output(&self) -> String {
        if let Some(text) = &self.view {
            return text.clone();
        }
        let on = self.merged.get() && self.auto_merge_takes;
        let request = if on {
            json!({ "enabledAt": "2026-10-20T00:00:00Z" })
        } else {
            Value::Null
        };
        json!({ "autoMergeRequest": request }).to_string()
    }

    fn diff_output(&self) -> String {
        if let Some(text) = &self.diff {
            return text.clone();
        }
        let staged = self.staged.borrow().clone().unwrap_or_default();
        format!(":160000 160000 {} {staged} M\0kbf\0", self.head.borrow())
    }

    /// The first line of each call (a commit message and a pull request body go on).
    fn log(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for call in self.calls.borrow().iter() {
            lines.push(call.lines().next().unwrap_or_default().to_owned());
        }
        lines
    }

    /// Whether some call holds `text`.
    fn did(&self, text: &str) -> bool {
        self.calls.borrow().iter().any(|c| c.contains(text))
    }

    /// The calls that start with `prefix`.
    fn starting(&self, prefix: &str) -> Vec<String> {
        let mut found = Vec::new();
        for call in self.log() {
            if call.starts_with(prefix) {
                found.push(call);
            }
        }
        found
    }

    /// The place of the first call that starts with `prefix`.
    fn at(&self, prefix: &str) -> usize {
        let log = self.log();
        for (i, call) in log.iter().enumerate() {
            if call.starts_with(prefix) {
                return i;
            }
        }
        panic!("no call starts with {prefix:?}: {log:?}");
    }

    /// The calls that are not a read of GitHub.
    fn writes(&self) -> Vec<String> {
        let reads = ["gh api ", "gh pr list ", "gh pr view "];
        let mut found = Vec::new();
        for call in self.log() {
            if !reads.iter().any(|r| call.starts_with(r)) {
                found.push(call);
            }
        }
        found
    }
}

/// Whether the run changed anything: the gitlink, a branch, a pull request.
fn changed(w: &World) -> bool {
    CHANGES.iter().any(|c| w.did(c))
}

impl Tools for World {
    fn gh(&self, args: &[&str]) -> Result<Ran, Error> {
        let line = self.record("gh", args);
        if let Some(error) = self.unstartable(&line) {
            return Err(error);
        }
        if let Some(broken) = self.broken(&line) {
            return Ok(broken);
        }
        let ran = match args {
            ["api", path] => self.api_answer(path),
            ["pr", "list", rest @ ..] => {
                let text = if rest.contains(&"--head") {
                    &self.own
                } else {
                    &self.prs
                };
                printed(text)
            }
            ["pr", "create", ..] => printed(&self.created),
            ["pr", "merge", ..] => {
                self.merged.set(true);
                printed("")
            }
            ["pr", "view", ..] => printed(&self.view_output()),
            ["pr", "close", ..] => printed(""),
            _ => panic!("unexpected gh call: {args:?}"),
        };
        Ok(ran)
    }

    fn git(&self, args: &[&str]) -> Result<Ran, Error> {
        let line = self.record("git", args);
        if let Some(error) = self.unstartable(&line) {
            return Err(error);
        }
        if let Some(broken) = self.broken(&line) {
            return Ok(broken);
        }
        let out = match args {
            ["clone", ..] | ["-C", _, "push", ..] => String::new(),
            ["-C", _, "ls-tree", ..] => match &self.tree {
                Some(text) => text.clone(),
                None => format!("160000 commit {}\tkbf\n", self.head.borrow()),
            },
            ["-C", _, "update-index", "--cacheinfo", info] => {
                let sha = info.split(',').nth(1).unwrap_or_default();
                *self.staged.borrow_mut() = Some(sha.to_owned());
                String::new()
            }
            ["-C", _, "diff", ..] => self.diff_output(),
            ["-C", _, "-c", _, "commit", ..] => {
                let staged = self.staged.borrow_mut().take();
                if let (Some(sha), true) = (staged, self.commit_moves_head) {
                    *self.head.borrow_mut() = sha;
                }
                String::new()
            }
            _ => panic!("unexpected git call: {args:?}"),
        };
        Ok(printed(&out))
    }
}

/// The login `gh` prints for the release bot as the author of a pull request.
const BOT: &str = "app/komira-release-bot";

/// One entry of `gh pr list` for the head branch `branch`, opened by `login`.
fn pr_by(login: &str, number: u64, branch: &str, state: &str, auto: bool) -> Value {
    let merge = if auto {
        json!({ "enabledAt": "2026-10-20T00:00:00Z" })
    } else {
        Value::Null
    };
    json!({
        "number": number,
        "state": state,
        "headRefName": branch,
        "url": format!("https://github.com/{INTERNAL}/pull/{number}"),
        "autoMergeRequest": merge,
        "author": { "login": login, "is_bot": true },
    })
}

/// One entry of `gh pr list` for the head branch `branch`, opened by the release bot.
fn pr_on(number: u64, branch: &str, state: &str, auto: bool) -> Value {
    pr_by(BOT, number, branch, state, auto)
}

/// One entry of `gh pr list` for the bump to `version`.
fn pr(number: u64, version: &str, state: &str, auto: bool) -> Value {
    pr_on(number, &format!("bump/kbf-v{version}"), state, auto)
}

fn listing(entries: &[Value]) -> String {
    Value::Array(entries.to_vec()).to_string()
}

/// `n` pull requests that are no bump.
fn many(n: u64) -> String {
    let mut entries = Vec::new();
    for i in 0..n {
        entries.push(pr_on(i, "feature/x", "OPEN", false));
    }
    listing(&entries)
}

fn bump_args(version: &str) -> BumpArgs {
    BumpArgs {
        repo: REPO.to_owned(),
        version: version.to_owned(),
        sha: NEW.to_owned(),
        internal_repo: INTERNAL.to_owned(),
        submodule: "kbf".to_owned(),
        workdir: Some(PathBuf::from(DIR)),
        gh: PathBuf::from("gh"),
        git: PathBuf::from("git"),
    }
}

fn request() -> Bump {
    Bump::from_args(&bump_args("1.2.3")).expect("a good request")
}

fn run(w: &World) -> Result<Vec<String>, Error> {
    apply(w, &request())
}

fn refused(result: Result<Vec<String>, Error>) -> String {
    match result {
        Err(Error::Refused(why)) => why.join("\n"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn failed(result: Result<Vec<String>, Error>) -> String {
    match result {
        Err(Error::Failed(why)) => why,
        other => panic!("expected a failure, got {other:?}"),
    }
}

/// The first line of the pull request's body, as it is in the commit message too.
fn body1() -> String {
    format!("Moves the `kbf` submodule to the commit of the release v1.2.3 ({NEW}).")
}

/// The calls of a bump that goes through, in order.
fn golden() -> Vec<String> {
    let body = body1();
    vec![
        format!("gh api repos/{REPO}/commits/refs/tags/v1.2.3"),
        format!(
            "gh pr list -R {INTERNAL} --state all --search head:bump/kbf-v sort:created-desc \
             --limit 100 --json number,state,headRefName,url,autoMergeRequest,author"
        ),
        format!(
            "gh pr list -R {INTERNAL} --state all --head {BRANCH} \
             --json number,state,headRefName,url,autoMergeRequest,author"
        ),
        format!("gh api repos/{INTERNAL}/git/ref/heads/{BRANCH}"),
        format!(
            "git clone --quiet --depth 1 --no-tags --single-branch --branch main \
             https://github.com/{INTERNAL}.git {DIR}"
        ),
        format!("git -C {DIR} ls-tree HEAD -- kbf"),
        format!("gh api repos/{REPO}/compare/{OLD}...{NEW}"),
        format!("git -C {DIR} update-index --cacheinfo 160000,{NEW},kbf"),
        format!(
            "git -C {DIR} diff --cached --raw -z --no-renames \
             --ignore-submodules=none HEAD"
        ),
        format!(
            "git -C {DIR} -c commit.gpgsign=false commit --quiet -m {TITLE} \
             -m {body}"
        ),
        format!("git -C {DIR} ls-tree HEAD -- kbf"),
        format!("git -C {DIR} push --quiet origin HEAD:refs/heads/{BRANCH}"),
        format!(
            "gh pr create -R {INTERNAL} --base main --head {BRANCH} --title {TITLE} \
             --body {body}"
        ),
        format!("gh pr merge {PR_URL} -R {INTERNAL} --auto --merge"),
        format!("gh pr view {PR_URL} -R {INTERNAL} --json autoMergeRequest"),
    ]
}

#[test]
fn a_bump_clones_commits_pushes_opens_and_turns_auto_merge_on() {
    // Catches: any drift in the calls a bump makes: a branch or title the guard does not
    // read as its own, a forced push, a clone of more than the tip, a pull request without
    // auto-merge, a missing check before the commit.
    let w = world();
    let said = run(&w).expect("a good bump");
    assert_eq!(w.log(), golden());
    let tagged = format!("tag v1.2.3 of {REPO} names {NEW}");
    let opened = format!("opened {PR_URL} with auto-merge: kbf to v1.2.3");
    assert_eq!(said, [tagged, opened]);
}

#[test]
fn a_missing_tag_refuses_before_anything_else() {
    // Catches: a bump of a commit the release never tagged (the guard's last rule).
    let mut w = world();
    w.api.remove(&tag_path());
    let want = format!("the tag v1.2.3 does not exist in {REPO}");
    assert_eq!(refused(run(&w)), want);
    assert_eq!(w.log(), [format!("gh api {}", tag_path())]);
}

#[test]
fn a_tag_the_commits_api_cannot_resolve_is_a_missing_tag() {
    // Catches: the 422 GitHub answers for a tag that does not exist (not a 404, as for every
    // other path) read as a failure of the call and not as the refusal it is.
    let mut w = world();
    w.api.remove(&tag_path());
    w.missing = "gh: No commit found for SHA: v1.2.3 (HTTP 422)";
    let want = format!("the tag v1.2.3 does not exist in {REPO}");
    assert_eq!(refused(run(&w)), want);
}

#[test]
fn a_tag_that_names_another_commit_refuses_before_anything_else() {
    // Catches: a tag check that accepts any tag, so a bump names a commit the tag does not.
    let mut w = world();
    w.set(&tag_path(), json!({ "sha": OTHER }));
    let want = format!("the tag v1.2.3 of {REPO} names {OTHER:?}, not {NEW}");
    assert_eq!(refused(run(&w)), want);
    assert_eq!(w.log(), [format!("gh api {}", tag_path())]);
}

#[test]
fn a_sha_that_is_not_ahead_of_the_pin_refuses_and_changes_nothing() {
    // Catches: a bump that moves the submodule backwards or sideways (accepting `behind`),
    // or one whose compare could not be made.
    for status in ["behind", "diverged"] {
        let mut w = world();
        w.set(&compare_path(OLD), json!({ "status": status }));
        let why = refused(run(&w));
        let says = format!("compare says {status}");
        assert!(why.contains(&says), "{why}");
        assert!(!changed(&w), "{status}");
    }
    let mut w = world();
    w.api.remove(&compare_path(OLD));
    let why = refused(run(&w));
    assert!(why.contains("compare says nothing"), "{why}");
    assert!(!changed(&w));
}

#[test]
fn a_pin_that_is_already_the_sha_opens_nothing() {
    // Catches: a pull request that moves the submodule to the commit it is at.
    let mut w = world();
    w.head = RefCell::new(NEW.to_owned());
    let said = run(&w).expect("nothing to do is not an error");
    let want = format!("kbf is already at {NEW}: nothing to open");
    assert_eq!(said[1], want);
    assert!(!changed(&w));
}

#[test]
fn a_newer_or_equal_bump_pull_request_is_a_no_op_with_no_write() {
    // Catches: a second pull request for a version one already covers, or an older bump
    // opened over a newer one (open or merged), and any write on the way to saying so.
    let covering = [
        pr(8, "1.2.4", "OPEN", true),
        pr(8, "1.2.4", "MERGED", false),
        pr(8, "1.2.3", "MERGED", false),
        pr(8, "1.2.3", "OPEN", true),
    ];
    for entry in covering {
        let name = entry.to_string();
        let mut w = world();
        w.prs = listing(&[entry]);
        let said = run(&w).expect("covered");
        let ends = said[1].ends_with("nothing to open");
        assert!(ends, "{name}: {said:?}");
        assert_eq!(w.writes(), Vec::<String>::new(), "{name}");
    }
}

#[test]
fn an_open_pull_request_for_this_version_without_auto_merge_gets_it() {
    // Catches: a run that stopped before turning auto-merge on and, run again, leaves the
    // pull request to be merged by hand.
    let mut w = world();
    w.prs = listing(&[pr(7, "1.2.3", "OPEN", false)]);
    let said = run(&w).expect("auto-merge turns on");
    let want = format!("{PR_URL} for v1.2.3 was open without auto-merge: turned it on");
    assert_eq!(said[1], want);
    let merge = format!("gh pr merge 7 -R {INTERNAL} --auto --merge");
    assert_eq!(w.writes(), [merge]);
}

#[test]
fn pull_requests_that_are_no_bump_of_this_submodule_cover_nothing() {
    // Catches: a closed pull request, another branch, another submodule's bump, a
    // prerelease or a malformed version read as a bump that covers (or is older than) this
    // one.
    let mut w = world();
    let entries = [
        pr(9, "1.2.9", "CLOSED", false),
        pr_on(10, "feature/x", "OPEN", false),
        pr_on(11, "bump/kbf-v1.2.9-rc1", "OPEN", false),
        pr_on(12, "bump/kbf-vendor", "OPEN", false),
        pr_on(13, "bump/mopm-v9.0.0", "OPEN", false),
        pr_on(14, "bump/kbf-v01.2.9", "OPEN", false),
        pr_on(15, "bump/kbf-v0.0.1", "CLOSED", false),
    ];
    w.prs = listing(&entries);
    let said = run(&w).expect("a good bump");
    assert!(said[1].starts_with("opened "), "{said:?}");
    assert!(w.did("gh pr create"));
    assert!(!w.did("gh pr close"));
}

#[test]
fn older_open_bump_pull_requests_are_closed_in_favour_of_the_new_one() {
    // Catches: older bump pull requests left open beside the new one, a merged one closed,
    // or a close before the new pull request exists.
    let mut w = world();
    let entries = [
        pr(5, "1.2.0", "OPEN", true),
        pr(4, "1.1.9", "OPEN", false),
        pr(3, "1.1.0", "MERGED", false),
    ];
    w.prs = listing(&entries);
    let said = run(&w).expect("a good bump");
    let note = "--comment Superseded by the bump to v1.2.3. --delete-branch";
    let close = |n: u32| format!("gh pr close {n} -R {INTERNAL} {note}");
    assert_eq!(w.starting("gh pr close"), [close(5), close(4)]);
    assert!(w.at("gh pr create") < w.at("gh pr close"));
    assert_eq!(said.len(), 4, "{said:?}");
    let want = "closed https://github.com/example-org/internal/pull/5 (older than v1.2.3)";
    assert_eq!(said[2], want);
}

#[test]
fn an_older_open_pull_request_is_closed_when_a_newer_one_covers_the_bump() {
    // Catches: older bumps left open because a newer one made this run a no-op.
    let mut w = world();
    let newer = pr(8, "1.2.9", "OPEN", true);
    let older = pr(5, "1.2.0", "OPEN", false);
    w.prs = listing(&[newer, older]);
    let said = run(&w).expect("covered");
    let ends = said[1].ends_with("nothing to open");
    assert!(ends, "{said:?}");
    let note = "--comment Superseded by the bump to v1.2.3. --delete-branch";
    let close = format!("gh pr close 5 -R {INTERNAL} {note}");
    assert_eq!(w.writes(), [close]);
}

#[test]
fn a_branch_that_already_holds_the_gitlink_is_reused() {
    // Catches: a run that pushed and stopped, run again, cloning and pushing a second time
    // (or failing on the existing branch) instead of going on to the pull request.
    let mut w = world();
    w.set(&ref_path(), json!({}));
    let held = json!({ "type": "submodule", "sha": NEW });
    w.set(&contents_path(), held);
    let said = run(&w).expect("a reused branch");
    assert!(said[1].starts_with("opened "), "{said:?}");
    assert_eq!(w.starting("git "), Vec::<String>::new());
    assert!(w.did("gh pr create") && w.did("gh pr merge"));
}

#[test]
fn a_branch_that_holds_another_gitlink_refuses() {
    // Catches: a reused branch that is not the release: the pull request would move the
    // submodule somewhere else.
    let other = json!({ "type": "submodule", "sha": OLD });
    let file = json!({ "type": "file", "sha": NEW });
    let cases = [Some(other), Some(file), None];
    for contents in cases {
        let mut w = world();
        w.set(&ref_path(), json!({}));
        if let Some(contents) = contents {
            w.set(&contents_path(), contents);
        }
        let why = refused(run(&w));
        let start = format!("the branch {BRANCH} of {INTERNAL}");
        assert!(why.starts_with(&start), "{why}");
        assert!(why.ends_with("delete the branch by hand"), "{why}");
        assert!(!changed(&w) && w.starting("git ").is_empty());
    }
}

#[test]
fn a_staged_change_that_is_not_one_gitlink_refuses_before_the_commit() {
    // Catches: a commit that carries anything beside the one gitlink (a second path, a file
    // in its place, another path, a deletion, a malformed diff), which the guard would refuse
    // after the push.
    let raws = [
        "",
        ":160000 160000 a b M\0kbf\0:100644 100644 c d M\0README\0",
        ":100644 100644 a b M\0kbf\0",
        ":160000 160000 a b M\0other\0",
        ":160000 160000 a b U\0kbf\0",
        ":160000 000000 a b D\0kbf\0",
        ":160000 160000 a b M\0kbf\0\0x",
        ":160000 160000 a b M",
    ];
    for raw in raws {
        let mut w = world();
        w.diff = Some(raw.to_owned());
        let why = refused(run(&w));
        let want = "not exactly one modified gitlink at kbf";
        assert!(why.contains(want), "{raw:?}: {why}");
        let committed = w.did("commit --quiet") || w.did("push --quiet");
        assert!(!committed, "{raw:?}");
        assert!(!w.did("gh pr create"), "{raw:?}");
    }
}

#[test]
fn a_commit_that_leaves_another_gitlink_is_an_error_before_the_push() {
    // Catches: a push of a branch whose commit does not hold the released commit.
    let mut w = world();
    w.commit_moves_head = false;
    let want = format!("after the commit the gitlink of kbf is {OLD}, not {NEW}");
    assert_eq!(failed(run(&w)), want);
    assert!(!w.did("push --quiet") && !w.did("gh pr create"));
}

#[test]
fn auto_merge_that_is_not_on_afterwards_is_an_error() {
    // Catches: a bump that reports success while the pull request waits to be merged by hand
    // (auto-merge refused by the repository, or not confirmed by what `gh pr view` prints).
    let mut w = world();
    w.auto_merge_takes = false;
    let want = format!("auto-merge is not on for {PR_URL} after `gh pr merge --auto --merge`");
    let why = failed(run(&w));
    assert_eq!(why, format!("{want} (already done: opened {PR_URL})"));
    let mut w = world();
    w.view = Some("not json".to_owned());
    let why = failed(run(&w));
    assert!(why.starts_with("auto-merge is not on for "), "{why}");
}

#[test]
fn a_full_page_of_pull_requests_is_not_an_error() {
    // Catches: a list that has reached its page size treated as a failure: every release
    // leaves a merged bump pull request behind, so the page fills for good, and the verb
    // would fail on every release from then on. The search is newest first, so a newer bump
    // cannot be hidden behind the page.
    let mut w = world();
    w.prs = many(100);
    let said = run(&w).expect("a full page is a list like any other");
    assert!(said[1].starts_with("opened "), "{said:?}");
    let lists = w.starting("gh pr list");
    assert!(
        lists[0].contains(" sort:created-desc --limit 100 "),
        "{lists:?}"
    );
}

#[test]
fn a_pull_request_listing_that_is_not_a_json_list_is_an_error() {
    // Catches: a garbled list read as "no pull requests", from the search or from the list
    // of the version's own branch.
    for text in ["not json", "{\"a\": 1}"] {
        let mut w = world();
        w.prs = text.to_owned();
        let why = failed(run(&w));
        let start = "gh pr list printed no JSON list";
        assert!(why.starts_with(start), "{why}");
        assert!(!changed(&w));
        let mut w = world();
        w.own = text.to_owned();
        let why = failed(run(&w));
        assert!(why.starts_with(start), "{why}");
        assert!(!changed(&w));
    }
}

#[test]
fn a_pull_request_the_search_has_not_indexed_yet_is_found_by_its_branch() {
    // Catches: a re-run right after a run that opened the pull request but stopped before
    // auto-merge, which the search does not list yet: the branch is reused and `gh pr create`
    // fails with "already exists". The list of the version's own branch finds it, and turns
    // auto-merge on.
    let mut w = world();
    w.own = listing(&[pr(7, "1.2.3", "OPEN", false)]);
    let said = run(&w).expect("auto-merge turns on");
    let want = format!("{PR_URL} for v1.2.3 was open without auto-merge: turned it on");
    assert_eq!(said[1], want);
    let merge = format!("gh pr merge 7 -R {INTERNAL} --auto --merge");
    assert_eq!(w.writes(), [merge]);
}

#[test]
fn the_list_of_the_own_branch_overrides_a_stale_search() {
    // Catches: a pull request the search still reads as open after it was merged, taken for
    // an open one without auto-merge (and given a `gh pr merge` that fails on a merged one):
    // the fresher state of the same number wins, and nothing is written.
    let mut w = world();
    w.prs = listing(&[pr(7, "1.2.3", "OPEN", false)]);
    w.own = listing(&[pr(7, "1.2.3", "MERGED", false)]);
    let said = run(&w).expect("covered");
    let want = format!("exists ({PR_URL}, merged): nothing to open");
    assert!(said[1].ends_with(&want), "{said:?}");
    assert_eq!(w.writes(), Vec::<String>::new());
}

/// An open pull request of a person, not of the release bot, for the bump to `version`.
fn human(number: u64, version: &str) -> Value {
    let branch = format!("bump/kbf-v{version}");
    pr_by("someone", number, &branch, "OPEN", false)
}

#[test]
fn only_pull_requests_of_the_release_bot_are_bumps() {
    // Catches: a stray pull request that happens to be named `bump/kbf-v9.9.9`, opened by
    // anyone, read as a bump that covers this one (the pin would stop for good, and every
    // run say "nothing to open"), and a person's older `bump/kbf-v*` pull request closed
    // and its branch deleted by the bot.
    let mut w = world();
    let stray = pr_by("someone", 9, "bump/kbf-v9.9.9", "MERGED", true);
    w.prs = listing(&[stray, human(5, "1.2.0")]);
    w.own = listing(&[human(6, "1.2.3")]);
    let said = run(&w).expect("a good bump");
    assert!(said[1].starts_with("opened "), "{said:?}");
    assert!(w.did("gh pr create"));
    assert!(!w.did("gh pr close"));
    let mut w = world();
    w.prs = listing(&[human(4, "1.1.0"), pr(5, "1.2.0", "OPEN", false)]);
    run(&w).expect("a good bump");
    assert_eq!(w.starting("gh pr close").len(), 1);
    assert!(w.did("gh pr close 5 "));
}

#[test]
fn a_bump_pull_request_without_number_or_url_is_an_error() {
    // Catches: an entry read with a default number, which would be closed or enabled as #0.
    let mut w = world();
    let bare = json!({ "headRefName": BRANCH, "state": "OPEN", "author": { "login": BOT } });
    w.prs = listing(&[bare]);
    let why = failed(run(&w));
    let start = "gh pr list printed an entry without number or url";
    assert!(why.starts_with(start), "{why}");
}

#[test]
fn what_gh_pr_create_prints_must_end_in_the_url() {
    // Catches: a pull request url taken from the wrong line, or none checked at all.
    let mut w = world();
    w.created = "created it\n".to_owned();
    let why = failed(run(&w));
    assert!(why.contains("not a pull request url"), "{why}");
    assert!(!w.did("gh pr merge"));
}

#[test]
fn trailing_blank_lines_after_the_pull_request_url_are_not_a_failure() {
    // Catches: a pull request that was created, and a run that reports failure because the
    // last line of what `gh pr create` printed is empty.
    let mut w = world();
    w.created = format!("Creating pull request\n\n{PR_URL}\n\n \n");
    let said = run(&w).expect("a good bump");
    assert_eq!(
        said[1],
        format!("opened {PR_URL} with auto-merge: kbf to v1.2.3")
    );
}

#[test]
fn a_gitlink_that_is_not_the_path_asked_for_or_not_a_full_commit_refuses() {
    // Catches: a `git ls-tree` line read as the gitlink of the submodule when it names
    // another path, or a commit id that is not 40 lowercase hex digits (the compare and the
    // commit would be made with it).
    let upper = OLD.replace('1', "A");
    let trees = [
        format!("160000 commit {OLD}\tother\n"),
        format!("160000 commit {OLD}\n"),
        format!("160000 commit {}\tkbf\n", &OLD[..39]),
        format!("160000 commit {upper}\tkbf\n"),
        format!("160000 commit {OLD}x\tkbf\n"),
        format!("100644 blob {OLD}\tkbf\n"),
        String::new(),
    ];
    for tree in trees {
        let mut w = world();
        w.tree = Some(tree.clone());
        let why = refused(run(&w));
        assert!(
            why.starts_with("kbf is not a gitlink at HEAD"),
            "{tree:?}: {why}"
        );
        assert!(!changed(&w), "{tree:?}");
    }
}

#[test]
fn a_failing_command_is_an_error_that_says_what_it_printed() {
    // Catches: a call whose failure is ignored, or read as an empty answer (no tag, no list,
    // no gitlink), which would let the bump go on.
    let needles = [
        "gh api repos/komira-ai/kbf/commits",
        "gh pr list",
        "--state all --head",
        "gh api repos/example-org/internal/git/ref",
        "git clone",
        "ls-tree",
        "gh api repos/komira-ai/kbf/compare",
        "update-index",
        "diff --cached",
        "commit --quiet",
        "push --quiet",
        "gh pr create",
    ];
    for needle in needles {
        let mut w = world();
        w.fail = Some((needle, "boom"));
        let why = failed(run(&w));
        assert!(why.ends_with("boom"), "{needle}: {why}");
    }
}

#[test]
fn a_failure_after_the_pull_request_is_opened_names_the_pull_request() {
    // Catches: a failed auto-merge (or its check) after `gh pr create` that leaves a pull
    // request open and an error that does not say where it is, so the job log hides it.
    for needle in ["gh pr merge", "gh pr view"] {
        let mut w = world();
        w.fail = Some((needle, "boom"));
        let why = failed(run(&w));
        let done = format!("(already done: opened {PR_URL})");
        assert!(why.contains(": boom"), "{needle}: {why}");
        assert!(why.ends_with(&done), "{needle}: {why}");
    }
}

#[test]
fn an_api_answer_that_is_not_json_is_an_error() {
    // Catches: an answer that cannot be read taken for an answer (an empty one, or "found").
    let mut w = world();
    w.garbage = Some("/commits/");
    let why = failed(run(&w));
    let start = "gh api repos/komira-ai/kbf/commits/refs/tags/v1.2.3: not JSON";
    assert!(why.starts_with(start), "{why}");
}

#[test]
fn a_failing_close_of_an_older_pull_request_is_an_error_that_names_the_new_one() {
    // Catches: a close that fails and is passed over, leaving the older bump open; and an
    // error that does not say that the new pull request was opened.
    let mut w = world();
    w.prs = listing(&[pr(5, "1.2.0", "OPEN", true)]);
    w.fail = Some(("gh pr close", "boom"));
    let why = failed(run(&w));
    assert!(
        why.starts_with("gh pr close: boom (already done: opened "),
        "{why}"
    );
    assert!(why.contains(PR_URL), "{why}");
}

#[test]
fn an_older_pull_request_merged_meanwhile_is_left_alone() {
    // Catches: a run that fails on `gh pr close` of a pull request the search still read as
    // open, after it was merged and the new pull request is open already.
    let mut w = world();
    w.prs = listing(&[pr(5, "1.2.0", "OPEN", true)]);
    let why = "X Pull request #5 can't be closed because it was already merged";
    w.fail = Some(("gh pr close", why));
    let said = run(&w).expect("a merged pull request needs no close");
    let want = "https://github.com/example-org/internal/pull/5 was merged meanwhile";
    assert!(said[2].starts_with(want), "{said:?}");
}

#[test]
fn a_program_that_cannot_be_started_is_an_error() {
    // Catches: a `gh` or `git` that is not there read as an empty answer (no tag, no list,
    // no clone), at each of the places that run one, and after the pull request is opened.
    let needles = [
        "gh api repos/komira-ai/kbf/commits",
        "gh pr list",
        "git clone",
        "gh pr create",
        "gh pr merge",
    ];
    for needle in needles {
        let mut w = world();
        w.spawn_fail = Some(needle);
        let got = run(&w);
        let spawned = matches!(got, Err(Error::Spawn { .. }));
        assert!(spawned, "{needle}: {got:?}");
    }
    let mut w = world();
    w.prs = listing(&[pr(5, "1.2.0", "OPEN", true)]);
    w.spawn_fail = Some("gh pr close");
    let got = run(&w);
    assert!(matches!(got, Err(Error::Spawn { .. })), "{got:?}");
}

#[test]
fn an_open_pull_request_whose_auto_merge_cannot_be_turned_on_is_an_error() {
    // Catches: a failed `gh pr merge` on an existing pull request passed over.
    let mut w = world();
    w.prs = listing(&[pr(7, "1.2.3", "OPEN", false)]);
    w.fail = Some(("gh pr merge", "boom"));
    assert_eq!(failed(run(&w)), "gh pr merge: boom");
}

#[test]
fn an_existing_branch_whose_gitlink_cannot_be_read_is_an_error() {
    // Catches: a branch that exists, whose contents cannot be read, taken for a branch to
    // reuse (or for none).
    let mut w = world();
    w.set(&ref_path(), json!({}));
    w.fail = Some(("gh api repos/example-org/internal/contents", "boom"));
    let why = failed(run(&w));
    assert!(why.ends_with("boom"), "{why}");
    assert!(!changed(&w));
}

#[test]
fn a_gitlink_that_cannot_be_read_after_the_commit_is_an_error_before_the_push() {
    // Catches: a push after a commit that was not looked at (the check after the commit
    // reads the tree again).
    let mut w = world();
    w.fail = Some(("ls-tree", "boom"));
    w.fail_after = 1;
    let why = failed(run(&w));
    assert!(why.ends_with("boom"), "{why}");
    assert!(w.did("commit --quiet") && !w.did("push --quiet"));
}
