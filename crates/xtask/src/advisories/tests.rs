//! The decisions of `advisories`, against a fake `cargo deny` and a fake GitHub: no
//! process, no network. Each test names the defect it catches.

use std::collections::BTreeMap;

use super::*;

const REPO: &str = "komira-ai/kbf";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const LIST: &str = "repos/komira-ai/kbf/issues?state=open&per_page=100&page=1";
const LABEL_PATH: &str = "repos/komira-ai/kbf/labels/security";

/// A line of `cargo deny --format json` for an advisory, in the shape cargo-deny 0.20.2
/// prints (docs of the module).
fn diagnostic(id: &str, krate: &str, version: &str, title: &str, url: Value) -> String {
    json!({
        "type": "diagnostic",
        "fields": {
            "code": "vulnerability",
            "severity": "error",
            "message": title,
            "labels": [{ "message": "security vulnerability detected", "span": krate }],
            "notes": [format!("ID: {id}")],
            "graphs": [{ "Krate": { "name": krate, "version": version }, "parents": [] }],
            "advisory": {
                "id": id, "package": krate, "title": title, "url": url,
                "date": "2026-01-01", "aliases": [], "categories": [], "keywords": [],
            },
        }
    })
    .to_string()
}

fn log(level: &str, message: &str) -> String {
    json!({ "type": "log", "fields": { "level": level, "message": message, "timestamp": "t" } })
        .to_string()
}

fn run_of(code: i32, lines: &[String]) -> DenyRun {
    DenyRun {
        code: Some(code),
        stderr: lines.join("\n") + "\n",
    }
}

fn one() -> Vec<String> {
    vec![diagnostic(
        "RUSTSEC-2026-0001",
        "tokio",
        "1.2.3",
        "Out of bounds",
        json!("https://rustsec.org/advisories/RUSTSEC-2026-0001"),
    )]
}

fn issue(number: u64, title: &str) -> Value {
    json!({ "number": number, "title": title })
}

/// `cargo deny` and GitHub as scripts: GET answers by path, every call recorded.
struct Fake {
    deny: Option<DenyRun>,
    /// Answer every write with nothing.
    silent: bool,
    gets: BTreeMap<String, Option<Value>>,
    calls: Vec<Call>,
}

fn fake(deny: DenyRun, issues: Vec<Value>) -> Fake {
    let mut gets = BTreeMap::new();
    gets.insert(LIST.to_owned(), Some(Value::Array(issues)));
    Fake {
        deny: Some(deny),
        silent: false,
        gets,
        calls: vec![],
    }
}

impl Fake {
    fn writes(&self) -> Vec<&Call> {
        self.calls
            .iter()
            .filter(|c| c.method != Method::Get)
            .collect()
    }
}

impl Tools for Fake {
    fn deny(&mut self) -> Result<DenyRun, Error> {
        Ok(self.deny.take().expect("cargo deny runs once"))
    }

    fn github(&mut self, call: &Call) -> Result<Option<Value>, Error> {
        self.calls.push(call.clone());
        if call.method == Method::Get {
            return Ok(self.gets.get(&call.path).cloned().expect("a scripted GET"));
        }
        if self.silent {
            return Ok(None);
        }
        Ok(Some(
            json!({ "number": 9, "html_url": "https://github.com/komira-ai/kbf/issues/9" }),
        ))
    }
}

fn failure(r: Result<Vec<String>, Error>) -> String {
    match r {
        Err(e) => e.to_string(),
        Ok(lines) => panic!("expected a failure, got {lines:?}"),
    }
}

// Catches: a run with nothing to report that creates an issue, or touches one.
#[test]
fn no_advisories_and_no_issue_does_nothing() {
    let mut f = fake(
        run_of(0, &[log("WARN", "a multiple-versions warning")]),
        vec![],
    );
    let out = sync(&mut f, REPO, COMMIT).expect("sync");
    assert_eq!(out, ["no advisories, and no open issue"]);
    assert!(f.writes().is_empty(), "{:?}", f.writes());
    assert_eq!(f.calls.len(), 1, "only the list of issues was read");
}

// Catches: no comment (the issue closes with no word of why), a close without the
// comment first, closing another issue, the wrong state, or a comment naming no commit.
#[test]
fn no_advisories_with_an_open_issue_comments_then_closes_it() {
    let mut f = fake(
        run_of(0, &[]),
        vec![issue(3, "Something else"), issue(7, TITLE)],
    );
    let out = sync(&mut f, REPO, COMMIT).expect("sync");
    assert_eq!(out, ["no advisories: closed issue #7"]);
    let base = "repos/komira-ai/kbf/issues/7";
    assert_eq!(
        f.writes(),
        [
            &Call::post(
                format!("{base}/comments"),
                json!({ "body": format!("No advisories as of {COMMIT}") })
            ),
            &Call::patch(
                base.to_owned(),
                json!({ "state": "closed", "state_reason": "completed" })
            ),
        ]
    );
}

// Catches: an issue created without the table (an id, crate, version, title or link
// missing), without the commit, or the label dropped when the repository has it.
#[test]
fn advisories_without_an_issue_create_it_with_a_table_and_the_label() {
    let mut f = fake(run_of(1, &one()), vec![issue(3, "Something else")]);
    f.gets
        .insert(LABEL_PATH.to_owned(), Some(json!({ "name": "security" })));
    let out = sync(&mut f, REPO, COMMIT).expect("sync");
    assert_eq!(
        out,
        ["1 advisories: opened https://github.com/komira-ai/kbf/issues/9"]
    );
    let [create] = f.writes()[..] else {
        panic!("one write: {:?}", f.writes())
    };
    assert_eq!(create.method, Method::Post);
    assert_eq!(create.path, "repos/komira-ai/kbf/issues");
    let body = create.body.clone().expect("a body");
    assert_eq!(body["title"], TITLE);
    assert_eq!(body["labels"], json!(["security"]));
    let text = body["body"].as_str().expect("text");
    assert!(text.contains(COMMIT), "{text}");
    assert!(
        text.contains(
            "| RUSTSEC-2026-0001 | tokio | 1.2.3 | Out of bounds | \
             https://rustsec.org/advisories/RUSTSEC-2026-0001 |"
        ),
        "{text}"
    );
}

// Catches: a label that is sent when the repository has none (GitHub would create it, or
// refuse the issue), so the issue is never opened.
#[test]
fn advisories_without_an_issue_or_the_label_create_it_unlabelled() {
    let mut f = fake(run_of(1, &one()), vec![]);
    f.gets.insert(LABEL_PATH.to_owned(), None);
    sync(&mut f, REPO, COMMIT).expect("sync");
    let [create] = f.writes()[..] else {
        panic!("one write")
    };
    assert_eq!(create.body.as_ref().expect("body")["labels"], json!([]));
}

// Catches: a second issue opened beside the first, a comment or a close on a run that
// found advisories, the title or labels overwritten, or a stale commit left in the body.
#[test]
fn advisories_with_an_issue_overwrite_its_body_only() {
    let mut f = fake(run_of(1, &one()), vec![issue(7, TITLE)]);
    let out = sync(&mut f, REPO, COMMIT).expect("sync");
    assert_eq!(out, ["1 advisories: updated issue #7"]);
    let [update] = f.writes()[..] else {
        panic!("one write: {:?}", f.writes())
    };
    assert_eq!(update.method, Method::Patch);
    assert_eq!(update.path, "repos/komira-ai/kbf/issues/7");
    let body = update.body.clone().expect("a body");
    assert_eq!(
        body.as_object().expect("object").len(),
        1,
        "only `body`: {body}"
    );
    assert_eq!(
        body["body"],
        render(&parse(&run_of(1, &one())).unwrap(), COMMIT)
    );
}

// Catches: a pull request with the issue's title taken for the issue (it would be
// "updated" or closed), and a search that stops at the first page.
#[test]
fn the_open_issue_is_found_on_a_later_page_and_a_pull_request_is_not_one() {
    let mut first: Vec<Value> = (1..=100).map(|n| issue(n, "Other")).collect();
    first[0] = json!({ "number": 1, "title": TITLE, "pull_request": {} });
    let mut f = fake(run_of(0, &[]), first);
    f.gets.insert(
        "repos/komira-ai/kbf/issues?state=open&per_page=100&page=2".to_owned(),
        Some(json!([issue(150, TITLE)])),
    );
    let out = sync(&mut f, REPO, COMMIT).expect("sync");
    assert_eq!(out, ["no advisories: closed issue #150"]);
}

// Catches: a missing repository or a reply that is not a list read as "no issue", which
// would open a duplicate or close nothing.
#[test]
fn an_unreadable_issue_list_is_a_failure() {
    let mut f = fake(run_of(0, &[]), vec![]);
    f.gets.insert(LIST.to_owned(), None);
    assert!(failure(sync(&mut f, REPO, COMMIT)).contains("not found"));
    f.deny = Some(run_of(0, &[]));
    f.gets
        .insert(LIST.to_owned(), Some(json!({ "message": "x" })));
    assert!(failure(sync(&mut f, REPO, COMMIT)).contains("not an array"));
    f.deny = Some(run_of(0, &[]));
    f.gets
        .insert(LIST.to_owned(), Some(json!([{ "title": TITLE }])));
    assert!(failure(sync(&mut f, REPO, COMMIT)).contains("no number"));
}

// Catches: output that is not cargo-deny's read as "no advisories" (a crash then closes
// the issue), and each shape of it that is not understood.
#[test]
fn malformed_cargo_deny_output_is_a_failure() {
    let bad = |stderr: &str, want: &str| {
        let run = DenyRun {
            code: Some(1),
            stderr: stderr.to_owned(),
        };
        let why = failure(sync(&mut fake(run, vec![]), REPO, COMMIT));
        assert!(why.contains(want), "{stderr:?}: {why}");
    };
    bad("thread 'main' panicked\n", "line 1");
    bad(&format!("{}\nnot json\n", one()[0]), "line 2");
    bad("{\"fields\":{}}\n", "no `type`");
    bad("{\"type\":\"diagnostic\"}\n", "no `fields`");
    let no_id =
        json!({ "type": "diagnostic", "fields": { "advisory": { "package": "a", "title": "t" } } });
    bad(&no_id.to_string(), "no string `id`");
    let yanked = json!({ "type": "diagnostic", "fields": { "code": "yanked", "message": "m" } });
    bad(&yanked.to_string(), "yanked crate with no graph");
}

// Catches: a cargo-deny that failed to run (bad config, no database) taken for a clean
// run, so the open issue is closed on a lie; and a clean run taken for a failure.
#[test]
fn a_tool_failure_is_a_failure_not_no_advisories() {
    let fails = |run: DenyRun, want: &str| {
        let why = failure(sync(&mut fake(run, vec![]), REPO, COMMIT));
        assert!(why.contains(want), "{why}");
    };
    fails(
        run_of(1, &[log("ERROR", "failed to fetch the database")]),
        "failed to fetch",
    );
    fails(
        run_of(1, &[log("WARN", "w")]),
        "exited 1 and reported no advisory",
    );
    fails(run_of(2, &[]), "exited 2");
    fails(
        DenyRun {
            code: None,
            stderr: String::new(),
        },
        "signal",
    );
    // A WARN log and a blank line are not failures.
    let ok = run_of(0, &[log("WARN", "w"), String::new(), log("INFO", "i")]);
    assert_eq!(parse(&ok).expect("parse"), []);
}

// Catches: a yanked crate dropped from the report (it failed the gate before this verb
// existed), a repeated advisory listed twice, an unsorted table, and a version taken from
// anywhere but the crate's graph node.
#[test]
fn parse_lists_each_advisory_once_sorted_with_yanked_crates_and_versions() {
    let yanked = json!({
        "type": "diagnostic",
        "fields": {
            "code": "yanked", "severity": "error",
            "message": "detected yanked crate (try `cargo update -p zip`)",
            "graphs": [{ "Krate": { "name": "zip", "version": "0.6.6" } }],
        }
    })
    .to_string();
    let other_kind =
        json!({ "type": "diagnostic", "fields": { "code": "license-not-encountered" } });
    let b = diagnostic("RUSTSEC-2026-0009", "b", "2.0.0", "B", Value::Null);
    let a1 = diagnostic("RUSTSEC-2026-0002", "a", "1.0.0", "A", json!("u"));
    let a2 = diagnostic("RUSTSEC-2026-0002", "a", "1.1.0", "A", json!("u"));
    let lines = [b, a1.clone(), a1, a2, yanked, other_kind.to_string()];
    let got = parse(&run_of(1, &lines)).expect("parse");
    let ids: Vec<(&str, &str, &str)> = got
        .iter()
        .map(|a| (a.id.as_str(), a.krate.as_str(), a.version.as_str()))
        .collect();
    assert_eq!(
        ids,
        [
            ("RUSTSEC-2026-0002", "a", "1.0.0"),
            ("RUSTSEC-2026-0002", "a", "1.1.0"),
            ("RUSTSEC-2026-0009", "b", "2.0.0"),
            ("yanked", "zip", "0.6.6"),
        ]
    );
    assert_eq!(got[2].url, None, "a null url is none");
}

// Catches: an advisory with no graph (the lock file's version unknown) dropped or failing
// the run.
#[test]
fn an_advisory_without_a_graph_has_an_unknown_version() {
    let line = json!({
        "type": "diagnostic",
        "fields": { "advisory": { "id": "RUSTSEC-1", "package": "p", "title": "t", "url": null } }
    });
    let got = parse(&run_of(1, &[line.to_string()])).expect("parse");
    assert_eq!(got[0].version, "unknown");
}

// Catches: a title with `|` or a newline breaking the table into more cells or rows.
#[test]
fn render_keeps_a_hostile_title_in_its_cell() {
    let a = Advisory {
        id: "RUSTSEC-1".to_owned(),
        krate: "c".to_owned(),
        version: "1.0.0".to_owned(),
        title: "a | b\nc".to_owned(),
        url: None,
    };
    let body = render(&[a], COMMIT);
    assert!(
        body.contains("| RUSTSEC-1 | c | 1.0.0 | a \\| b c |  |\n"),
        "{body}"
    );
}

// Catches: the body sent to `gh` on stdin without `--input -`: gh ignores it, so every
// issue would be created empty (or the update would send nothing).
#[test]
fn a_body_goes_with_input_dash_and_a_get_has_none() {
    assert_eq!(
        gh_args(&Call::get("repos/o/r/issues".to_owned())),
        ["api", "repos/o/r/issues"]
    );
    let post = Call::post("repos/o/r/issues".to_owned(), json!({}));
    assert_eq!(
        gh_args(&post),
        ["api", "-X", "POST", "repos/o/r/issues", "--input", "-"]
    );
    let patch = Call::patch("repos/o/r/issues/1".to_owned(), json!({}));
    assert_eq!(
        gh_args(&patch),
        ["api", "-X", "PATCH", "repos/o/r/issues/1", "--input", "-"]
    );
}

// Catches: a repository that is not `<owner>/<repo>` reaching a URL.
#[test]
fn a_repository_is_owner_slash_name() {
    assert!(check_repo("komira-ai/kbf").is_ok());
    for bad in [
        "kbf", "a/b/c", "/kbf", "a/", "a b/c", "a/..", "../b", "a/b?x",
    ] {
        let r = check_repo(bad);
        assert!(matches!(r, Err(Error::Usage(_))), "{bad}: {r:?}");
    }
}

// Catches: a write that GitHub (or `gh`) answered with nothing taken for a success that
// has its issue number, or a crash.
#[test]
fn a_write_with_no_reply_is_a_failure() {
    let mut f = fake(run_of(1, &one()), vec![]);
    f.gets.insert(LABEL_PATH.to_owned(), None);
    f.silent = true;
    assert!(failure(sync(&mut f, REPO, COMMIT)).contains("no reply"));
}
