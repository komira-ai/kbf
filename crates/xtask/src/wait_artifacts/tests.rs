use serde_json::json;

use super::*;

/// A GitHub that answers each look with the next scripted pair of replies (jobs, artifacts),
/// the last pair for every look after the script, and counts the pauses.
struct Script {
    looks: Vec<(Value, Value)>,
    asked: Vec<String>,
    pauses: usize,
    fail: bool,
}

impl Script {
    fn new(looks: Vec<(Value, Value)>) -> Self {
        Script {
            looks,
            asked: Vec::new(),
            pauses: 0,
            fail: false,
        }
    }
}

impl Github for Script {
    fn get(&mut self, path: &str) -> Result<Value, Error> {
        if self.fail {
            return Err(Error::Failed("gh api: down".to_owned()));
        }
        self.asked.push(path.to_owned());
        let look = (self.asked.len() - 1) / 2;
        let pair = &self.looks[look.min(self.looks.len() - 1)];
        Ok(if path.contains("/jobs?") {
            pair.0.clone()
        } else {
            pair.1.clone()
        })
    }

    fn pause(&mut self) {
        self.pauses += 1;
    }
}

fn args(wanted: &[&str], polls: u32) -> Args {
    Args {
        repo: "o/r".to_owned(),
        run_id: 7,
        artifacts: wanted.iter().map(|w| (*w).to_owned()).collect(),
        job_prefix: "test-shard".to_owned(),
        poll_seconds: 0,
        polls,
        gh: PathBuf::from("gh"),
    }
}

fn jobs(list: &[(&str, Value)]) -> Value {
    json!({ "jobs": list.iter().map(|(n, c)| json!({ "name": n, "conclusion": c })).collect::<Vec<_>>() })
}

fn made(names: &[&str]) -> Value {
    json!({ "artifacts": names.iter().map(|n| json!({ "name": n, "expired": false })).collect::<Vec<_>>() })
}

fn running() -> Value {
    jobs(&[
        ("test-shard (legacy 1/2)", Value::Null),
        ("test-shard (js 1/1)", json!("success")),
        ("test", Value::Null),
    ])
}

const WANTED: [&str; 3] = ["profraw-legacy-1", "profraw-legacy-2", "profraw-js-1"];

/// Catches: a wait that returns before every named artifact is there, one that keeps looking
/// after they are, one that pauses before the first look, and the paths asked.
#[test]
fn returns_when_every_named_artifact_is_there() {
    let mut s = Script::new(vec![
        (running(), made(&["profraw-js-1"])),
        (running(), made(&["profraw-js-1", "profraw-legacy-1"])),
        (
            running(),
            made(&[
                "profraw-js-1",
                "profraw-legacy-1",
                "profraw-legacy-2",
                "coverage",
            ]),
        ),
    ]);
    let line = wait(&mut s, &args(&WANTED, 10)).expect("there");
    assert_eq!(
        line,
        "wait-artifacts: the 3 artifact(s) are there: profraw-legacy-1, profraw-legacy-2, profraw-js-1"
    );
    assert_eq!(s.pauses, 2);
    assert_eq!(s.asked.len(), 6);
    assert_eq!(s.asked[0], "repos/o/r/actions/runs/7/jobs?per_page=100");
    assert_eq!(
        s.asked[1],
        "repos/o/r/actions/runs/7/artifacts?per_page=100"
    );

    let mut s = Script::new(vec![(running(), made(&WANTED))]);
    wait(&mut s, &args(&WANTED, 10)).expect("there at once");
    assert_eq!(s.pauses, 0);
}

/// Catches: an expired artifact, one of another name (a prefix of a wanted name, or the
/// wanted name with more after it), or one without a name counted as a wanted one; a reply
/// with no list taken for one with every artifact; and the artifacts still missing not named
/// when the looks are used up.
#[test]
fn counts_only_live_artifacts_of_the_names_asked_for() {
    let odd = json!({ "artifacts": [
        { "name": "profraw-legacy-1", "expired": true },
        { "name": "profraw-legacy-2-more" },
        { "name": "profraw-legacy" },
        { "expired": false },
        { "name": "profraw-js-1" },
    ]});
    let mut s = Script::new(vec![(running(), odd)]);
    let e = wait(&mut s, &args(&WANTED, 2)).expect_err("two of three missing");
    assert_eq!(
        e.to_string(),
        "after 2 look(s) at the run 2 of 3 artifact(s) have not come: profraw-legacy-1, profraw-legacy-2"
    );
    assert_eq!(s.pauses, 1);

    let mut s = Script::new(vec![(running(), json!({}))]);
    let e = wait(&mut s, &args(&["a"], 1)).expect_err("none");
    assert_eq!(
        e.to_string(),
        "after 1 look(s) at the run 1 of 1 artifact(s) have not come: a"
    );
    assert!(s.asked.len() == 2);

    // No look at all: nothing is known to be there.
    let mut s = Script::new(vec![(running(), made(&["a"]))]);
    let e = wait(&mut s, &args(&["a"], 0)).expect_err("no looks");
    assert_eq!(
        e.to_string(),
        "after 0 look(s) at the run 1 of 1 artifact(s) have not come: a"
    );
    assert!(s.asked.is_empty());
}

/// Catches: a failed, cancelled or timed-out shard left waited for until the limit, a shard
/// that is still running or was skipped, or a job of another name, taken for a failure; the
/// failed job not named; and the artifacts looked at after a failure.
#[test]
fn a_shard_that_did_not_succeed_ends_the_wait() {
    for conclusion in ["failure", "cancelled", "timed_out"] {
        let bad = jobs(&[
            ("lint", json!("failure")),
            ("ci / test-shard (legacy 2/2)", json!(conclusion)),
        ]);
        let mut s = Script::new(vec![(bad, made(&[]))]);
        let e = wait(&mut s, &args(&WANTED, 5)).expect_err("failed");
        assert_eq!(
            e.to_string(),
            format!(
                "ci / test-shard (legacy 2/2) ({conclusion}) did not succeed, so its artifact will not come"
            )
        );
        assert_eq!(s.asked.len(), 1, "no look at the artifacts after a failure");
    }
    let mut fine = jobs(&[
        ("lint", json!("failure")),
        ("test-shard (legacy 1/2)", json!("success")),
        ("test-shard (legacy 2/2)", Value::Null),
        ("test-shard (js 1/1)", json!(7)),
        ("", json!("failure")),
    ]);
    fine["jobs"]
        .as_array_mut()
        .expect("jobs")
        .push(json!({ "conclusion": "failure" }));
    let mut s = Script::new(vec![(fine, made(&WANTED))]);
    wait(&mut s, &args(&WANTED, 1)).expect("none of those failed a shard");
}

/// Catches: a wait that holds a runner for the whole limit when every shard has finished and
/// an artifact never came (an upload that was skipped); and the reverse, a run in which no
/// shard job is listed yet, or one that is still running, cut short.
#[test]
fn every_shard_finished_without_an_artifact_ends_the_wait() {
    let done = jobs(&[
        ("test-shard (legacy 1/1)", json!("success")),
        ("test-shard (js 1/1)", json!("skipped")),
        ("test", Value::Null),
    ]);
    let mut s = Script::new(vec![(done, made(&["profraw-js-1"]))]);
    let e = wait(&mut s, &args(&["profraw-legacy-1", "profraw-js-1"], 9)).expect_err("never");
    assert_eq!(
        e.to_string(),
        "every job named test-shard* has finished and these artifacts did not come: profraw-legacy-1"
    );
    assert_eq!(s.asked.len(), 2);

    for listed in [jobs(&[]), jobs(&[("test", Value::Null)]), running()] {
        let mut s = Script::new(vec![(listed, made(&[]))]);
        let e = wait(&mut s, &args(&["x"], 3)).expect_err("still waiting");
        assert!(e.to_string().starts_with("after 3 look(s)"), "{e}");
        assert_eq!(s.pauses, 2);
    }
}

/// Catches: a failed call to GitHub read as an empty answer.
#[test]
fn a_call_that_fails_fails_the_wait() {
    let mut s = Script::new(vec![(running(), made(&[]))]);
    s.fail = true;
    let e = wait(&mut s, &args(&WANTED, 3)).expect_err("down");
    assert_eq!(e.to_string(), "gh api: down");
}
