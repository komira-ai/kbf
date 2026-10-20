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
    /// What `gh pr list` prints.
    prs: String,
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
    /// The gitlink `git update-index` staged.
    staged: RefCell<Option<String>>,
    /// Whether `gh pr merge` ran.
    merged: Cell<bool>,
    /// Every call: the tool, then its arguments joined by spaces.
    calls: RefCell<Vec<String>>,
}

fn tag_path() -> String {
    format!("repos/{REPO}/commits/v1.2.3")
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
        created: format!("Creating pull request\n\n{PR_URL}\n"),
        auto_merge_takes: true,
        view: None,
        garbage: None,
        missing: "gh: Not Found (HTTP 404)",
        head: RefCell::new(OLD.to_owned()),
        commit_moves_head: true,
        diff: None,
        fail: None,
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
        line.contains(needle).then(|| broke(why))
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
        if let Some(broken) = self.broken(&line) {
            return Ok(broken);
        }
        let ran = match args {
            ["api", path] => self.api_answer(path),
            ["pr", "list", ..] => printed(&self.prs),
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
        if let Some(broken) = self.broken(&line) {
            return Ok(broken);
        }
        let out = match args {
            ["clone", ..] | ["-C", _, "push", ..] => String::new(),
            ["-C", _, "ls-tree", ..] => format!("160000 commit {}\tkbf\n", self.head.borrow()),
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

/// One entry of `gh pr list` for the head branch `branch`.
fn pr_on(number: u64, branch: &str, state: &str, auto: bool) -> Value {
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
    })
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
        format!("gh api repos/{REPO}/commits/v1.2.3"),
        format!(
            "gh pr list -R {INTERNAL} --state all --search head:bump/kbf-v --limit 500 \
             --json number,state,headRefName,url,autoMergeRequest"
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
fn the_commit_and_the_pull_request_name_the_release() {
    // Catches: a pull request that does not say which release it moves the submodule to.
    let body = request().body();
    let link = "https://github.com/komira-ai/kbf/releases/tag/v1.2.3";
    assert!(body.starts_with(&body1()), "{body}");
    assert!(body.contains(link), "{body}");
    assert!(body.contains("bump guard"), "{body}");
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
    assert_eq!(failed(run(&w)), want);
    let mut w = world();
    w.view = Some("not json".to_owned());
    let why = failed(run(&w));
    assert!(why.starts_with("auto-merge is not on for "), "{why}");
}

#[test]
fn a_pull_request_listing_that_reaches_the_limit_is_an_error() {
    // Catches: a list cut at the limit read as complete, so a newer bump hidden behind it
    // is opened over; and a limit that is off by one.
    let mut w = world();
    w.prs = many(500);
    let why = failed(run(&w));
    let start = "gh pr list returned 500 pull requests";
    assert!(why.starts_with(start), "{why}");
    assert!(!changed(&w));
    let mut w = world();
    w.prs = many(499);
    let said = run(&w).expect("a list below the limit is complete");
    assert!(said[1].starts_with("opened "), "{said:?}");
}

#[test]
fn a_pull_request_listing_that_is_not_a_json_list_is_an_error() {
    // Catches: a garbled list read as "no pull requests".
    for text in ["not json", "{\"a\": 1}"] {
        let mut w = world();
        w.prs = text.to_owned();
        let why = failed(run(&w));
        let start = "gh pr list printed no JSON list";
        assert!(why.starts_with(start), "{why}");
        assert!(!changed(&w));
    }
}

#[test]
fn a_bump_pull_request_without_number_or_url_is_an_error() {
    // Catches: an entry read with a default number, which would be closed or enabled as #0.
    let mut w = world();
    w.prs = listing(&[json!({ "headRefName": BRANCH, "state": "OPEN" })]);
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
fn a_failing_command_is_an_error_that_says_what_it_printed() {
    // Catches: a call whose failure is ignored, or read as an empty answer (no tag, no list,
    // no gitlink), which would let the bump go on.
    let needles = [
        "gh api repos/komira-ai/kbf/commits",
        "gh pr list",
        "gh api repos/example-org/internal/git/ref",
        "git clone",
        "ls-tree",
        "gh api repos/komira-ai/kbf/compare",
        "update-index",
        "diff --cached",
        "commit --quiet",
        "push --quiet",
        "gh pr create",
        "gh pr merge",
        "gh pr view",
    ];
    for needle in needles {
        let mut w = world();
        w.fail = Some((needle, "boom"));
        let why = failed(run(&w));
        assert!(why.ends_with("boom"), "{needle}: {why}");
    }
}

#[test]
fn an_api_answer_that_is_not_json_is_an_error() {
    // Catches: an answer that cannot be read taken for an answer (an empty one, or "found").
    let mut w = world();
    w.garbage = Some("/commits/");
    let why = failed(run(&w));
    let start = "gh api repos/komira-ai/kbf/commits/v1.2.3: not JSON";
    assert!(why.starts_with(start), "{why}");
}

#[test]
fn a_failing_close_of_an_older_pull_request_is_an_error() {
    // Catches: a close that fails and is passed over, leaving the older bump open.
    let mut w = world();
    w.prs = listing(&[pr(5, "1.2.0", "OPEN", true)]);
    w.fail = Some(("gh pr close", "boom"));
    assert_eq!(failed(run(&w)), "gh pr close: boom");
}

fn args_with(flag: &str, value: &str) -> BumpArgs {
    let mut a = bump_args("1.2.3");
    match flag {
        "--version" => a.version = value.to_owned(),
        "--sha" => a.sha = value.to_owned(),
        "--repo" => a.repo = value.to_owned(),
        "--internal-repo" => a.internal_repo = value.to_owned(),
        "--submodule" => a.submodule = value.to_owned(),
        _ => panic!("no such flag {flag}"),
    }
    a
}

#[test]
fn values_the_guard_would_refuse_are_usage_errors() {
    // Catches: a version, commit, repository or submodule name that is not what the guard
    // and the GitHub paths built from it assume, accepted and sent on.
    let upper = "ABCDEF0123456789ABCDEF0123456789ABCDEF01";
    let bad = [
        ("--version", "v1.2.3"),
        ("--version", "1.2"),
        ("--version", "1.2.3.4.5"),
        ("--version", "01.2.3"),
        ("--version", "1.2.3-rc1"),
        ("--version", ""),
        ("--sha", "abc"),
        ("--sha", &NEW[..39]),
        ("--sha", upper),
        ("--repo", "kbf"),
        ("--repo", "a/b/c"),
        ("--repo", "/kbf"),
        ("--repo", "x/"),
        ("--repo", ".x/y"),
        ("--repo", "x/.y"),
        ("--repo", "x y/z"),
        ("--internal-repo", "internal"),
        ("--internal-repo", "a/b/c"),
        ("--submodule", ""),
        ("--submodule", ".kbf"),
        ("--submodule", "a/b"),
        ("--submodule", "a b"),
    ];
    for (flag, value) in bad {
        let got = Bump::from_args(&args_with(flag, value));
        let Err(Error::Usage(why)) = got else {
            panic!("{flag} {value:?} was accepted");
        };
        assert!(why.starts_with(flag), "{flag} {value:?}: {why}");
    }
}

#[test]
fn a_run_with_no_workdir_gets_one_of_its_own() {
    // Catches: a default clone directory shared by two runs, or none.
    let mut a = bump_args("10.20.30");
    a.workdir = None;
    let b = Bump::from_args(&a).expect("a good request");
    let dir = b.workdir.to_string_lossy().into_owned();
    let own = format!("xtask-bump-{}", std::process::id());
    assert!(dir.contains(&own), "{dir}");
    assert_eq!(request().workdir, PathBuf::from(DIR));
}

/// The bump guard's rule for a branch: `bump/<submodule>-v<semver>`, plain numbers.
fn guard_accepts(branch: &str, submodule: &str) -> bool {
    let Some(semver) = branch.strip_prefix(&format!("bump/{submodule}-v")) else {
        return false;
    };
    let parts: Vec<&str> = semver.split('.').collect();
    let plain = |p: &&str| p.parse::<u64>().is_ok_and(|n| n.to_string() == **p);
    parts.len() == 3 && parts.iter().all(plain)
}

#[test]
fn the_branch_and_the_title_are_the_ones_the_bump_guard_accepts() {
    // Catches: a branch or title that drifts from `bump/<submodule>-v<semver>` and
    // `chore(<submodule>): bump to v<semver>`: the guard reads the branch, and the pull
    // requests already merged read the title.
    for version in ["0.0.1", "1.2.3", "10.20.30"] {
        let b = Bump::from_args(&bump_args(version)).expect("a good request");
        assert_eq!(b.branch(), format!("bump/kbf-v{version}"));
        assert_eq!(b.title(), format!("chore(kbf): bump to v{version}"));
        assert_eq!(b.tag(), format!("v{version}"));
        assert!(guard_accepts(&b.branch(), "kbf"), "{}", b.branch());
    }
    assert!(!guard_accepts("bump/kbf-v1.2", "kbf"));
    assert!(!guard_accepts("bump/mopm-v1.2.3", "kbf"));
    assert!(!guard_accepts("bump/kbf-v01.2.3", "kbf"));
}
