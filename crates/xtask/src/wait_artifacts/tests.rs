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

fn args(of: usize, polls: u32) -> Args {
    Args {
        repo: "o/r".to_owned(),
        run_id: 7,
        artifact_prefix: "profraw-".to_owned(),
        job_prefix: "test-shard".to_owned(),
        of,
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
        ("test-shard (1)", Value::Null),
        ("test-shard (3)", Value::Null),
    ])
}

/// Catches: a wait that returns before the other parts' artifacts are there, one that keeps
/// looking after they are, one that pauses before the first look, and the paths asked.
#[test]
fn returns_when_every_other_part_has_uploaded() {
    let mut s = Script::new(vec![
        (running(), made(&["profraw-1"])),
        (running(), made(&["profraw-1"])),
        (running(), made(&["profraw-1", "profraw-2", "coverage"])),
    ]);
    let line = wait(&mut s, &args(3, 10)).expect("there");
    assert_eq!(
        line,
        "wait-artifacts: 2 artifact(s) named profraw-* are there (wanted 2)"
    );
    assert_eq!(s.pauses, 2);
    assert_eq!(s.asked.len(), 6);
    assert_eq!(s.asked[0], "repos/o/r/actions/runs/7/jobs?per_page=100");
    assert_eq!(
        s.asked[1],
        "repos/o/r/actions/runs/7/artifacts?per_page=100"
    );

    let mut s = Script::new(vec![(running(), made(&["profraw-1", "profraw-2"]))]);
    wait(&mut s, &args(3, 10)).expect("there at once");
    assert_eq!(s.pauses, 0);
}

/// Catches: an expired artifact, one of another name, or an artifact without a name
/// counted as a part's; a reply with no list taken for one with enough; and a one-part run
/// waiting for anything.
#[test]
fn counts_only_live_artifacts_with_the_prefix() {
    let odd = json!({ "artifacts": [
        { "name": "profraw-1", "expired": true },
        { "name": "other-2" },
        { "expired": false },
        { "name": "profraw-3" },
    ]});
    let mut s = Script::new(vec![(running(), odd)]);
    let e = wait(&mut s, &args(3, 2)).expect_err("one of two");
    assert_eq!(
        e.to_string(),
        "after 2 look(s) at the run only 1 of 2 artifact(s) named profraw-* have come"
    );
    assert_eq!(s.pauses, 1);

    let mut s = Script::new(vec![(json!({}), json!({}))]);
    let e = wait(&mut s, &args(2, 1)).expect_err("none");
    assert!(e.to_string().contains("only 0 of 1"), "{e}");
    wait(&mut s, &args(1, 1)).expect("nothing to wait for");
    wait(&mut s, &args(0, 1)).expect("nothing to wait for");
}

/// Catches: a failed, cancelled or timed-out part left waited for until the limit, a part
/// that is still running or was skipped, or a job of another name, taken for a failure; and
/// the failed job not named.
#[test]
fn a_part_that_did_not_succeed_ends_the_wait() {
    for conclusion in ["failure", "cancelled", "timed_out"] {
        let bad = jobs(&[
            ("lint", json!("failure")),
            ("ci / test-shard (2)", json!(conclusion)),
        ]);
        let mut s = Script::new(vec![(bad, made(&[]))]);
        let e = wait(&mut s, &args(3, 5)).expect_err("failed");
        assert_eq!(
            e.to_string(),
            format!(
                "ci / test-shard (2) ({conclusion}) did not succeed, so its profraw-* artifact will not come"
            )
        );
        assert_eq!(s.asked.len(), 1, "no look at the artifacts after a failure");
    }
    let mut fine = jobs(&[
        ("lint", json!("failure")),
        ("test-shard (1)", json!("success")),
        ("test-shard (2)", json!("skipped")),
        ("test-shard (3)", Value::Null),
        ("test-shard (4)", json!(7)),
        ("", json!("failure")),
    ]);
    fine["jobs"]
        .as_array_mut()
        .expect("jobs")
        .push(json!({ "conclusion": "failure" }));
    let mut s = Script::new(vec![(fine, made(&["profraw-1", "profraw-2"]))]);
    wait(&mut s, &args(3, 1)).expect("none of those failed a part");
}

/// Catches: a failed call to GitHub read as an empty answer.
#[test]
fn a_call_that_fails_fails_the_wait() {
    let mut s = Script::new(vec![(running(), made(&[]))]);
    s.fail = true;
    let e = wait(&mut s, &args(3, 3)).expect_err("down");
    assert_eq!(e.to_string(), "gh api: down");
}
