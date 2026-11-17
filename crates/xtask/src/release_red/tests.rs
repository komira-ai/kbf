//! The decisions of `release-red`, against a fake GitHub: no process, no network.

use std::collections::BTreeMap;

use super::*;
use crate::advisories::Method;

const REPO: &str = "komira-ai/kbf";
const HEAD: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const GREEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const RUN_URL: &str = "https://github.com/komira-ai/kbf/actions/runs/77";
const RUN: &str = "repos/komira-ai/kbf/actions/runs/77";
const JOBS: &str = "repos/komira-ai/kbf/actions/runs/77/jobs?per_page=100&filter=latest&page=1";
const JOBS2: &str = "repos/komira-ai/kbf/actions/runs/77/jobs?per_page=100&filter=latest&page=2";
const ISSUES: &str = "repos/komira-ai/kbf/issues?state=open&labels=main-red&per_page=100&page=1";
const ISSUES2: &str = "repos/komira-ai/kbf/issues?state=open&labels=main-red&per_page=100&page=2";
const RUNS: &str = "repos/komira-ai/kbf/actions/workflows/5/runs?branch=main&event=push&status=success&per_page=20";
const COMPARE: &str = "repos/komira-ai/kbf/compare/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa...bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb?per_page=100";
const NEWEST: &str =
    "repos/komira-ai/kbf/actions/workflows/5/runs?branch=main&event=push&per_page=1";
const LABEL_PATH: &str = "repos/komira-ai/kbf/labels/main-red";
const ANNOTATIONS: &str = "repos/komira-ai/kbf/check-runs/101/annotations";

struct Fake {
    gets: BTreeMap<String, Option<Value>>,
    /// Paths whose call fails with HTTP 502.
    broken: Vec<String>,
    /// Answer every write with nothing.
    silent: bool,
    calls: Vec<Call>,
}

fn fake() -> Fake {
    let mut f = Fake {
        gets: BTreeMap::new(),
        broken: vec![],
        silent: false,
        calls: vec![],
    };
    f.set(RUN, run_json("push", "main"));
    f.set(ISSUES, json!([]));
    f.set(RUNS, json!({ "workflow_runs": [] }));
    f.set(NEWEST, json!({ "workflow_runs": [{ "id": 77 }] }));
    f
}

impl Fake {
    fn set(&mut self, path: &str, reply: Value) {
        self.gets.insert(path.to_owned(), Some(reply));
    }

    fn missing(&mut self, path: &str) {
        self.gets.insert(path.to_owned(), None);
    }

    fn jobs(&mut self, jobs: &[(&str, &str, &str)]) {
        self.set(JOBS, jobs_json(jobs));
    }

    fn writes(&self) -> Vec<&Call> {
        self.calls
            .iter()
            .filter(|c| c.method != Method::Get)
            .collect()
    }
}

impl Github for Fake {
    fn call(&mut self, call: &Call) -> Result<Option<Value>, Error> {
        self.calls.push(call.clone());
        if self.broken.contains(&call.path) {
            return Err(Error::Failed(format!("gh api {}: HTTP 502", call.path)));
        }
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

fn run_json(event: &str, branch: &str) -> Value {
    json!({
        "html_url": RUN_URL, "head_sha": HEAD, "head_branch": branch, "event": event,
        "workflow_id": 5, "name": "suites", "created_at": "2026-11-02T10:00:00Z",
    })
}

/// `(id is the position + 100, name, status/conclusion)`: a conclusion of `running` is a job
/// that has not completed.
fn jobs_json(jobs: &[(&str, &str, &str)]) -> Value {
    let list: Vec<Value> = jobs
        .iter()
        .enumerate()
        .map(|(n, (name, status, conclusion))| {
            json!({ "id": 101 + n, "name": name, "status": status,
                    "conclusion": if conclusion.is_empty() { Value::Null } else { json!(conclusion) } })
        })
        .collect();
    json!({ "total_count": list.len(), "jobs": list })
}

fn commit(message: &str) -> Value {
    json!({ "commit": { "message": message } })
}

fn failure(r: Result<Vec<String>, Error>) -> String {
    match r {
        Err(e) => e.to_string(),
        Ok(lines) => panic!("expected a failure, got {lines:?}"),
    }
}

fn body_of(call: &Call) -> String {
    call.body.as_ref().expect("a body")["body"]
        .as_str()
        .expect("text")
        .to_owned()
}

// Catches: a run of a pull request or of another branch opening an issue.
#[test]
fn a_run_that_is_not_a_push_to_main_changes_nothing() {
    for (event, branch) in [("pull_request", "main"), ("push", "feature")] {
        let mut f = fake();
        f.set(RUN, run_json(event, branch));
        let lines = sync(&mut f, REPO, 77).expect("sync");
        assert_eq!(
            lines,
            [format!(
                "run 77 is a {event} on {branch}, not a push to main: nothing changed"
            )]
        );
        assert_eq!(f.calls.len(), 1);
    }
}

// Catches: a green run with no issue writing anything.
#[test]
fn green_with_no_open_issue_writes_nothing() {
    let mut f = fake();
    f.jobs(&[
        ("a", "completed", "success"),
        ("b", "completed", "skipped"),
        ("c", "completed", "neutral"),
        ("reporter", "in_progress", ""),
    ]);
    assert_eq!(
        sync(&mut f, REPO, 77).expect("sync"),
        ["green, and no open issue"]
    );
    assert!(f.writes().is_empty());
}

// Catches: closing before commenting, a comment other than the `Fixed:` line, a close that
// is not `completed`.
#[test]
fn green_with_an_open_issue_comments_fixed_then_closes() {
    let mut f = fake();
    f.jobs(&[("a", "completed", "success")]);
    f.set(
        ISSUES,
        json!([{ "number": 7, "title": "RELEASE-RED: kbf beta / x" }]),
    );
    assert_eq!(
        sync(&mut f, REPO, 77).expect("sync"),
        ["green: closed issue #7"]
    );
    let writes = f.writes();
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[0].path, "repos/komira-ai/kbf/issues/7/comments");
    assert_eq!(writes[0].method, Method::Post);
    assert_eq!(
        body_of(writes[0]),
        format!("Fixed: suites is green at {HEAD} ({RUN_URL})")
    );
    assert_eq!(writes[1].method, Method::Patch);
    assert_eq!(writes[1].path, "repos/komira-ai/kbf/issues/7");
    assert_eq!(
        writes[1].body,
        Some(json!({ "state": "closed", "state_reason": "completed" }))
    );
}

// Catches: a cancelled run (or an unknown conclusion) closing the issue or opening one.
#[test]
fn a_cancelled_job_leaves_everything_alone() {
    for conclusion in ["cancelled", "action_required", "stale"] {
        let mut f = fake();
        f.jobs(&[
            ("a", "completed", "success"),
            ("b", "completed", conclusion),
        ]);
        let lines = sync(&mut f, REPO, 77).expect("sync");
        assert_eq!(
            lines,
            ["run 77 has a job that was cancelled or is undecided: nothing changed"]
        );
        assert!(f.writes().is_empty());
    }
}

// Catches: a cancelled job hiding a failure that is in the same run.
#[test]
fn a_failure_wins_over_a_cancelled_job() {
    let mut f = fake();
    f.jobs(&[
        ("a", "completed", "cancelled"),
        ("b", "completed", "timed_out"),
    ]);
    f.set(ANNOTATIONS.replace("101", "102").as_str(), json!([]));
    f.set(LABEL_PATH, json!({}));
    assert_eq!(
        sync(&mut f, REPO, 77).expect("sync"),
        ["red: opened https://github.com/komira-ai/kbf/issues/9"]
    );
    assert_eq!(
        f.writes()[0].body.as_ref().expect("body")["title"],
        "RELEASE-RED: kbf b"
    );
}

// Catches: the issue body not in the alert reader's format, a title not from the first
// failing job, a missing label, the label created when it exists.
#[test]
fn red_with_no_issue_opens_one_in_the_alert_format() {
    let mut f = fake();
    f.jobs(&[
        ("a", "completed", "success"),
        ("beta / x", "completed", "failure"),
        ("beta / y", "completed", "timed_out"),
        ("reporter", "in_progress", ""),
    ]);
    f.set(LABEL_PATH, json!({ "name": "main-red" }));
    f.set(
        "repos/komira-ai/kbf/check-runs/102/annotations",
        json!([{ "message": "Process completed\nwith exit code 1.\u{1b}" }]),
    );
    let lines = sync(&mut f, REPO, 77).expect("sync");
    assert_eq!(
        lines,
        ["red: opened https://github.com/komira-ai/kbf/issues/9"]
    );
    let writes = f.writes();
    assert_eq!(writes.len(), 1, "the label exists: no label write");
    assert_eq!(writes[0].path, "repos/komira-ai/kbf/issues");
    let body = writes[0].body.as_ref().expect("body");
    assert_eq!(body["title"], "RELEASE-RED: kbf beta / x");
    assert_eq!(body["labels"], json!(["main-red"]));
    assert_eq!(
        body["body"],
        format!(
            "Run: {RUN_URL}\nHead: {HEAD}\nLast green: none\nCulprits: unknown\n\
             Failing: beta / x\nFailing: beta / y\nLog: Process completed with exit code 1."
        )
    );
}

// Catches: an issue opened under a label the repository does not have.
#[test]
fn a_missing_label_is_created_before_the_issue() {
    let mut f = fake();
    f.jobs(&[("a", "completed", "failure")]);
    f.set(ANNOTATIONS, json!([]));
    f.missing(LABEL_PATH);
    sync(&mut f, REPO, 77).expect("sync");
    let writes = f.writes();
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[0].path, "repos/komira-ai/kbf/labels");
    assert_eq!(
        writes[0].body,
        Some(json!({
            "name": "main-red", "color": "b60205",
            "description": "The release on main is red",
        }))
    );
    assert_eq!(writes[1].path, "repos/komira-ai/kbf/issues");
}

// Catches: a second issue for every red run, an edit of the open issue's title or body.
#[test]
fn red_with_an_open_issue_comments_the_same_lines() {
    let mut f = fake();
    f.jobs(&[("a", "completed", "failure")]);
    f.set(ANNOTATIONS, json!([]));
    f.set(
        ISSUES,
        json!([{ "number": 7, "title": "RELEASE-RED: kbf old" }]),
    );
    assert_eq!(
        sync(&mut f, REPO, 77).expect("sync"),
        ["red: commented on issue #7"]
    );
    let writes = f.writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].method, Method::Post);
    assert_eq!(writes[0].path, "repos/komira-ai/kbf/issues/7/comments");
    assert_eq!(
        body_of(writes[0]),
        format!("Run: {RUN_URL}\nHead: {HEAD}\nLast green: none\nCulprits: unknown\nFailing: a")
    );
}

// Catches: a pull request with the title, or an issue of another title, taken for the
// issue; the second page of issues not read.
#[test]
fn the_issue_is_found_past_pull_requests_other_titles_and_page_one() {
    let mut f = fake();
    f.jobs(&[("a", "completed", "success")]);
    let mut page: Vec<Value> = (0..PAGE)
        .map(|n| json!({ "number": 1000 + n, "title": "something else" }))
        .collect();
    page[0] = json!({ "number": 3, "title": "RELEASE-RED: kbf x", "pull_request": {} });
    page[1] = json!({ "number": 4, "title": null });
    f.set(ISSUES, Value::Array(page));
    f.set(
        ISSUES2,
        json!([{ "number": 8, "title": "RELEASE-RED: kbf late" }]),
    );
    assert_eq!(
        sync(&mut f, REPO, 77).expect("sync"),
        ["green: closed issue #8"]
    );
}

// Catches: a pull request titled like the issue standing for it.
#[test]
fn a_pull_request_with_the_title_is_not_the_issue() {
    let mut f = fake();
    f.jobs(&[("a", "completed", "success")]);
    f.set(
        ISSUES,
        json!([{ "number": 3, "title": "RELEASE-RED: kbf x", "pull_request": {} }]),
    );
    assert_eq!(
        sync(&mut f, REPO, 77).expect("sync"),
        ["green, and no open issue"]
    );
    assert!(f.writes().is_empty());
}

// Catches: jobs past the first page ignored, a failure on page two missed.
#[test]
fn jobs_are_read_from_every_page() {
    let mut f = fake();
    let page: Vec<(String, &str, &str)> = (0..PAGE)
        .map(|n| (format!("job{n}"), "completed", "success"))
        .collect();
    let page: Vec<(&str, &str, &str)> = page.iter().map(|(n, s, c)| (n.as_str(), *s, *c)).collect();
    f.jobs(&page);
    f.set(JOBS2, jobs_json(&[("late", "completed", "failure")]));
    f.set(LABEL_PATH, json!({}));
    f.set("repos/komira-ai/kbf/check-runs/101/annotations", json!([]));
    sync(&mut f, REPO, 77).expect("sync");
    assert_eq!(
        f.writes()[0].body.as_ref().expect("body")["title"],
        "RELEASE-RED: kbf late"
    );
}

// Catches: more than five Failing lines, more than two Log lines, a Log line over 300
// characters or with a newline, an empty annotation taking a Log slot.
#[test]
fn the_failing_and_log_lines_are_capped() {
    let mut f = fake();
    let names: Vec<String> = (0..7).map(|n| format!("j{n}")).collect();
    let jobs: Vec<(&str, &str, &str)> = names
        .iter()
        .map(|n| (n.as_str(), "completed", "failure"))
        .collect();
    f.jobs(&jobs);
    f.set(LABEL_PATH, json!({}));
    let long = "x".repeat(400);
    f.set(
        ANNOTATIONS,
        json!([{ "message": "  " }, { "message": long }, { "message": "b\tc" }, { "message": "d" }, { "other": 1 }]),
    );
    sync(&mut f, REPO, 77).expect("sync");
    assert_eq!(
        body_of(f.writes()[0]),
        format!(
            "Run: {RUN_URL}\nHead: {HEAD}\nLast green: none\nCulprits: unknown\n\
             Failing: j0\nFailing: j1\nFailing: j2\nFailing: j3\nFailing: j4\n\
             Log: {}\nLog: b c",
            "x".repeat(300)
        )
    );
}

// Catches: no annotations (a 404) failing the report.
#[test]
fn a_job_with_no_annotations_has_no_log_line() {
    let mut f = fake();
    f.jobs(&[("a", "completed", "failure")]);
    f.set(LABEL_PATH, json!({}));
    f.missing(ANNOTATIONS);
    sync(&mut f, REPO, 77).expect("sync");
    assert!(!body_of(f.writes()[0]).contains("Log:"));
}

fn red_with_green(f: &mut Fake, runs: Value) {
    f.jobs(&[("a", "completed", "failure")]);
    f.set(LABEL_PATH, json!({}));
    f.set(ANNOTATIONS, json!([]));
    f.set(RUNS, json!({ "workflow_runs": runs }));
}

fn runs_before_this_one() -> Value {
    json!([
        { "head_sha": "ccc", "created_at": "2026-11-02T11:00:00Z" },
        { "head_sha": HEAD, "created_at": "2026-11-02T09:00:00Z" },
        { "head_sha": GREEN, "created_at": "2026-11-02T08:00:00Z" },
        { "head_sha": "ddd", "created_at": "2026-11-01T08:00:00Z" },
    ])
}

// Catches: the last green taken from a run newer than this one, or at the same head; the
// culprits not read from the compare, duplicates kept, `(#N)` taken from the middle of a
// subject, order lost.
#[test]
fn last_green_skips_newer_and_same_head_runs_and_culprits_come_from_the_compare() {
    let mut f = fake();
    red_with_green(&mut f, runs_before_this_one());
    f.set(
        COMPARE,
        json!({ "total_commits": 6, "commits": [
            commit("Add a thing (#12)\n\nbody (#99)"),
            commit("Fix (#13)  "),
            commit("Again (#12)"),
            commit("Mention (#14) in the middle"),
            commit("Not a number (#x)"),
            commit("Empty (#)"),
        ] }),
    );
    sync(&mut f, REPO, 77).expect("sync");
    assert_eq!(
        body_of(f.writes()[0]),
        format!(
            "Run: {RUN_URL}\nHead: {HEAD}\nLast green: {GREEN}\nCulprits: #12, #13\nFailing: a"
        )
    );
}

// Catches: more than 20 culprits listed, or the oldest 20 kept instead of the 20 nearest
// the red head.
#[test]
fn culprits_are_capped_at_the_newest_twenty() {
    let mut f = fake();
    red_with_green(&mut f, runs_before_this_one());
    let commits: Vec<Value> = (1..=25).map(|n| commit(&format!("c (#{n})"))).collect();
    f.set(COMPARE, json!({ "total_commits": 25, "commits": commits }));
    sync(&mut f, REPO, 77).expect("sync");
    let want: Vec<String> = (6..=25).map(|n| format!("#{n}")).collect();
    assert!(body_of(f.writes()[0]).contains(&format!("Culprits: {}\n", want.join(", "))));
}

// Catches: culprits guessed from a compare that lists fewer commits than it counts, from
// subjects with no number, or from a green commit GitHub no longer has.
#[test]
fn culprits_are_unknown_when_the_compare_cannot_say() {
    let cases = [
        Some(json!({ "total_commits": 150, "commits": [commit("x (#1)")] })),
        Some(json!({ "total_commits": 1, "commits": [commit("no number")] })),
        Some(json!({ "total_commits": 0, "commits": [] })),
        None,
    ];
    for case in cases {
        let mut f = fake();
        red_with_green(&mut f, runs_before_this_one());
        match case {
            Some(v) => f.set(COMPARE, v),
            None => f.missing(COMPARE),
        }
        sync(&mut f, REPO, 77).expect("sync");
        assert!(
            body_of(f.writes()[0]).contains(&format!("Last green: {GREEN}\nCulprits: unknown\n"))
        );
    }
}

// Catches: a last green invented when no earlier run succeeded.
#[test]
fn no_earlier_green_run_is_none() {
    let mut f = fake();
    red_with_green(
        &mut f,
        json!([{ "head_sha": "ccc", "created_at": "2026-11-02T11:00:00Z" },
               { "created_at": "2026-11-01T11:00:00Z" },
               { "head_sha": "eee" }]),
    );
    sync(&mut f, REPO, 77).expect("sync");
    assert!(body_of(f.writes()[0]).contains("Last green: none\nCulprits: unknown\n"));
}

// Catches: GitHub failures swallowed on a read the decision rests on, so a red run goes
// unreported or an issue is closed on a run nobody read.
#[test]
fn github_failures_propagate() {
    let paths = [RUN, JOBS, ISSUES];
    for broken in paths {
        let mut f = fake();
        red_with_green(&mut f, runs_before_this_one());
        f.set(COMPARE, json!({ "total_commits": 0, "commits": [] }));
        f.broken.push(broken.to_owned());
        assert_eq!(
            failure(sync(&mut f, REPO, 77)),
            format!("gh api {broken}: HTTP 502"),
        );
        assert!(f.writes().is_empty());
    }
    let mut f = fake();
    f.jobs(&[("a", "completed", "success")]);
    f.set(
        ISSUES,
        json!([{ "number": 7, "title": "RELEASE-RED: kbf a" }]),
    );
    f.broken.push(NEWEST.to_owned());
    assert_eq!(
        failure(sync(&mut f, REPO, 77)),
        format!("gh api {NEWEST}: HTTP 502")
    );
    assert!(f.writes().is_empty());
    for write_at in 0..4 {
        // The label, the issue; the comment and the close of a green run.
        let mut f = fake();
        if write_at < 2 {
            f.jobs(&[("a", "completed", "failure")]);
            f.missing(LABEL_PATH);
            f.set(ANNOTATIONS, json!([]));
        } else {
            f.jobs(&[("a", "completed", "success")]);
            f.set(
                ISSUES,
                json!([{ "number": 7, "title": "RELEASE-RED: kbf a" }]),
            );
        }
        let target = ["labels", "issues", "issues/7/comments", "issues/7"][write_at];
        f.broken.push(format!("repos/komira-ai/kbf/{target}"));
        assert_eq!(
            failure(sync(&mut f, REPO, 77)),
            format!("gh api repos/komira-ai/kbf/{target}: HTTP 502")
        );
    }
}

// Catches: a write answered with nothing taken for success.
#[test]
fn a_write_with_no_reply_is_an_error() {
    let mut f = fake();
    f.jobs(&[("a", "completed", "failure")]);
    f.set(LABEL_PATH, json!({}));
    f.set(ANNOTATIONS, json!([]));
    f.silent = true;
    assert_eq!(
        failure(sync(&mut f, REPO, 77)),
        "gh api repos/komira-ai/kbf/issues: no reply"
    );
    let mut f = fake();
    f.jobs(&[("a", "completed", "failure")]);
    f.missing(LABEL_PATH);
    f.set(ANNOTATIONS, json!([]));
    f.silent = true;
    assert_eq!(
        failure(sync(&mut f, REPO, 77)),
        "gh api repos/komira-ai/kbf/labels: no reply"
    );
}

// Catches: answers GitHub does not give in the shape read being taken as empty.
#[test]
fn malformed_answers_are_errors() {
    let mut f = fake();
    f.missing(RUN);
    assert_eq!(
        failure(sync(&mut f, REPO, 77)),
        format!("gh api {RUN}: not found")
    );
    for key in [
        "html_url",
        "head_sha",
        "head_branch",
        "event",
        "name",
        "created_at",
        "workflow_id",
    ] {
        let mut f = fake();
        let mut run = run_json("push", "main");
        run.as_object_mut().expect("object").remove(key);
        f.set(RUN, run);
        assert!(failure(sync(&mut f, REPO, 77)).contains(key));
    }
    let mut f = fake();
    f.set(JOBS, json!({ "nope": 1 }));
    assert_eq!(
        failure(sync(&mut f, REPO, 77)),
        format!("gh api {JOBS}: not a list")
    );
    let mut f = fake();
    f.jobs(&[("a", "completed", "success")]);
    f.set(ISSUES, json!({}));
    assert_eq!(
        failure(sync(&mut f, REPO, 77)),
        format!("gh api {ISSUES}: not a list")
    );
    f.set(ISSUES, json!([{ "title": "RELEASE-RED: kbf a" }]));
    assert_eq!(
        failure(sync(&mut f, REPO, 77)),
        format!("gh api {ISSUES}: an issue with no number")
    );
    let mut f = fake();
    f.jobs(&[("a", "completed", "success")]);
    f.set(
        ISSUES,
        json!([{ "number": 7, "title": "RELEASE-RED: kbf a" }]),
    );
    f.set(NEWEST, json!({}));
    assert_eq!(
        failure(sync(&mut f, REPO, 77)),
        format!("gh api {NEWEST}: not a list")
    );
}

// Catches: the alert lost because its context could not be read (a token without
// `checks: read` is refused the annotations): the issue is still opened, with `none`,
// `unknown` and no `Log` line, and each refused read is printed.
#[test]
fn a_refused_read_of_the_context_still_opens_the_issue() {
    let mut f = fake();
    red_with_green(&mut f, runs_before_this_one());
    f.set(LABEL_PATH, json!({}));
    f.broken.extend([RUNS.to_owned(), ANNOTATIONS.to_owned()]);
    assert_eq!(
        sync(&mut f, REPO, 77).expect("sync"),
        [
            format!("could not read the last green run: gh api {RUNS}: HTTP 502"),
            format!("could not read the annotations: gh api {ANNOTATIONS}: HTTP 502"),
            "red: opened https://github.com/komira-ai/kbf/issues/9".to_owned(),
        ]
    );
    assert_eq!(
        body_of(f.writes()[0]),
        format!("Run: {RUN_URL}\nHead: {HEAD}\nLast green: none\nCulprits: unknown\nFailing: a")
    );
    // A compare refused (not a 404) and a last green that is not a list are kept the same way.
    let mut f = fake();
    red_with_green(&mut f, runs_before_this_one());
    f.set(
        ISSUES,
        json!([{ "number": 7, "title": "RELEASE-RED: kbf a" }]),
    );
    f.broken.push(COMPARE.to_owned());
    assert_eq!(
        sync(&mut f, REPO, 77).expect("sync"),
        [
            format!("could not read the culprits: gh api {COMPARE}: HTTP 502"),
            "red: commented on issue #7".to_owned(),
        ]
    );
    assert_eq!(
        body_of(f.writes()[0]),
        format!("Run: {RUN_URL}\nHead: {HEAD}\nLast green: {GREEN}\nCulprits: unknown\nFailing: a")
    );
    let mut f = fake();
    red_with_green(&mut f, json!([]));
    f.set(RUNS, json!({}));
    f.set(LABEL_PATH, json!({}));
    assert_eq!(
        sync(&mut f, REPO, 77).expect("sync")[0],
        format!("could not read the last green run: gh api {RUNS}: not a list")
    );
}

// Catches: a green re-run of an older run closing the issue while a newer run (red, or
// still running) speaks for main; and a newest-run list with no run, or no id, keeping the
// issue open for ever.
#[test]
fn only_the_newest_run_closes_the_issue() {
    let mut f = fake();
    f.jobs(&[("a", "completed", "success")]);
    f.set(
        ISSUES,
        json!([{ "number": 7, "title": "RELEASE-RED: kbf a" }]),
    );
    f.set(NEWEST, json!({ "workflow_runs": [{ "id": 78 }] }));
    assert_eq!(
        sync(&mut f, REPO, 77).expect("sync"),
        ["green, but run 78 is newer: issue #7 is left to it"]
    );
    assert!(f.writes().is_empty());
    for newest in [json!([]), json!([{ "name": "no id" }])] {
        let mut f = fake();
        f.jobs(&[("a", "completed", "success")]);
        f.set(
            ISSUES,
            json!([{ "number": 7, "title": "RELEASE-RED: kbf a" }]),
        );
        f.set(NEWEST, json!({ "workflow_runs": newest }));
        assert_eq!(
            sync(&mut f, REPO, 77).expect("sync"),
            ["green: closed issue #7"]
        );
    }
}

// Catches: a repository name that is not one path segment pair reaching `gh`.
#[test]
fn a_bad_repo_is_a_usage_error() {
    let args = Args {
        repo: "komira-ai".to_owned(),
        run_id: 1,
        gh: PathBuf::from("gh"),
    };
    assert!(matches!(run(&args), Err(Error::Usage(_))));
}

// Catches: a subject's number read from anywhere but its end.
#[test]
fn pull_number_reads_only_a_trailing_number() {
    assert_eq!(pull_number("Add (#12)"), Some("#12".to_owned()));
    assert_eq!(pull_number("Add (#12) more"), None);
    assert_eq!(pull_number("Add (12)"), None);
    assert_eq!(pull_number("Add #12)"), None);
    assert_eq!(pull_number("Add (#)"), None);
    assert_eq!(pull_number("Add (#1a)"), None);
    assert_eq!(pull_number("Add"), None);
}
