use serde_json::json;

use super::*;

/// A GitHub that answers each look with the next scripted pair of replies (jobs, artifacts),
/// the last pair for every look after the script, and counts the pauses.
struct Script {
    looks: Vec<(Value, Value)>,
    asked: Vec<String>,
    pauses: usize,
    /// Always fail.
    fail: bool,
    /// Whether each call, in order, fails; the calls after the list succeed.
    flaky: Vec<bool>,
}

impl Script {
    fn new(looks: Vec<(Value, Value)>) -> Self {
        Script {
            looks,
            asked: Vec::new(),
            pauses: 0,
            fail: false,
            flaky: Vec::new(),
        }
    }
}

impl Github for Script {
    fn get(&mut self, path: &str) -> Result<Value, Error> {
        if self.fail || (!self.flaky.is_empty() && self.flaky.remove(0)) {
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
    assert_eq!(
        s.asked[0],
        "repos/o/r/actions/runs/7/jobs?filter=latest&per_page=100"
    );
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

/// Catches: a failed call to GitHub read as an empty answer, and a call that keeps failing
/// waited out to the end of the looks (the run holds a runner) instead of ending the wait.
#[test]
fn a_call_that_keeps_failing_fails_the_wait() {
    let mut s = Script::new(vec![(running(), made(&[]))]);
    s.fail = true;
    let e = wait(&mut s, &args(&WANTED, 100)).expect_err("down");
    assert_eq!(e.to_string(), "gh api: down");
    assert_eq!(s.pauses, FAILED_LOOKS as usize - 1);
}

/// Catches: a limit on failed looks in a row so low that a short outage of GitHub, after twenty
/// minutes of build, fails the gate: nineteen failed looks in a row are waited out, and the
/// twentieth ends the wait. Counted in looks, the unit of the limit, not in time.
#[test]
fn an_outage_of_nineteen_looks_is_waited_out() {
    let mut s = Script::new(vec![(running(), made(&WANTED))]);
    s.flaky = vec![true; 19];
    wait(&mut s, &args(&WANTED, 100)).expect("the twentieth look works");
    assert_eq!(s.pauses, 19);

    let mut s = Script::new(vec![(running(), made(&WANTED))]);
    s.flaky = vec![true; 20];
    let e = wait(&mut s, &args(&WANTED, 100)).expect_err("twenty in a row");
    assert_eq!(e.to_string(), "gh api: down");
    assert_eq!(s.pauses, 19);
}

/// Catches: one bad reply from GitHub (a 502 failed a run of the gate that was an hour from its
/// artifacts) ending the wait; the limit on failed looks in a row not being a limit; and the
/// count of failed looks not starting again after a look that worked.
#[test]
fn a_look_that_fails_is_tried_again() {
    let few = FAILED_LOOKS as usize - 1;
    let mut s = Script::new(vec![(running(), made(&WANTED))]);
    s.flaky = vec![true; few];
    wait(&mut s, &args(&WANTED, 100)).expect("the look after the failed ones works");
    assert_eq!(s.pauses, few);

    let mut s = Script::new(vec![(running(), made(&WANTED))]);
    s.flaky = vec![true; few + 1];
    let e = wait(&mut s, &args(&WANTED, 100)).expect_err("too many in a row");
    assert_eq!(e.to_string(), "gh api: down");

    // The artifacts call failing after the jobs call worked is a failed look as well.
    let mut s = Script::new(vec![(running(), made(&WANTED))]);
    s.flaky = vec![false, true];
    wait(&mut s, &args(&WANTED, 100)).expect("the look after the failed one works");
    assert_eq!(s.pauses, 1);

    // Failures, a look that works but finds an artifact missing, and failures again: no more
    // than the limit in a row at any time, though more in all.
    let mut flaky = vec![true; few];
    flaky.extend([false, false]);
    flaky.extend(vec![true; few]);
    let mut s = Script::new(vec![(running(), made(&[])), (running(), made(&WANTED))]);
    s.flaky = flaky;
    wait(&mut s, &args(&WANTED, 100)).expect("never too many in a row");
}

const T0: &str = "2026-10-01T09:00:00Z";
/// When the first attempt's shards made their artifacts.
const T1: &str = "2026-10-01T10:00:00Z";
/// When the second attempt's jobs started.
const T2: &str = "2026-10-01T11:00:00Z";
/// After the second attempt's jobs started.
const T3: &str = "2026-10-01T11:30:00Z";

/// A job as the jobs reply lists it: the attempt it last ran in and when it started.
fn job(name: &str, conclusion: Value, attempt: u64, started: &str) -> Value {
    json!({ "name": name, "conclusion": conclusion, "run_attempt": attempt, "started_at": started })
}

fn listed(jobs: &[Value]) -> Value {
    json!({ "jobs": jobs })
}

fn art(name: &str, created: &str) -> Value {
    json!({ "name": name, "expired": false, "created_at": created })
}

fn artifacts(list: &[Value]) -> Value {
    json!({ "artifacts": list })
}

/// What the gate asks for: each artifact with the text of its shard's job name.
const KEYED: [&str; 3] = [
    "profraw-legacy-1=(legacy-1-of-2)",
    "profraw-legacy-2=(legacy-2-of-2)",
    "profraw-js-1=(js-1-of-1)",
];

fn shard_names() -> [&'static str; 3] {
    [
        "test-shard (legacy-1-of-2)",
        "test-shard (legacy-2-of-2)",
        "test-shard (js-1-of-1)",
    ]
}

/// Catches: "Re-run all jobs" read as done at once. Attempt 1's artifacts are still there while
/// every shard runs again, so the wait returned and `test` downloaded stale profiles; a shard
/// that then failed did not fail `test`. Each artifact counts only when it was made after its
/// shard's job of this attempt started; the wait goes on until all three are.
#[test]
fn rerun_of_every_job_waits_for_this_attempts_artifacts() {
    let [l1, l2, js] = shard_names();
    let running = || {
        let rows: Vec<Value> = [l1, l2, js]
            .iter()
            .map(|n| job(n, Value::Null, 2, T2))
            .chain([job("test", Value::Null, 2, T2)])
            .collect();
        listed(&rows)
    };
    let stale = |name: &str| art(name, T1);
    let mut s = Script::new(vec![
        (
            running(),
            artifacts(&[
                stale("profraw-legacy-1"),
                stale("profraw-legacy-2"),
                stale("profraw-js-1"),
            ]),
        ),
        (
            running(),
            artifacts(&[
                art("profraw-legacy-1", T3),
                stale("profraw-legacy-2"),
                stale("profraw-js-1"),
            ]),
        ),
        (
            running(),
            artifacts(&[
                art("profraw-legacy-1", T3),
                art("profraw-legacy-2", T3),
                art("profraw-js-1", T2),
            ]),
        ),
    ]);
    let line = wait(&mut s, &args(&KEYED, 10)).expect("all three of this attempt");
    assert_eq!(s.pauses, 2, "two looks found attempt 1's artifacts only");
    assert_eq!(
        line,
        "wait-artifacts: the 3 artifact(s) are there: profraw-legacy-1, profraw-legacy-2, profraw-js-1",
        "the jobs' text is not part of the printed names"
    );

    // The looks used up on attempt 1's artifacts name what is missing, without the job text.
    let mut s = Script::new(vec![(
        running(),
        artifacts(&[
            stale("profraw-legacy-1"),
            art("profraw-legacy-2", T3),
            stale("profraw-js-1"),
        ]),
    )]);
    let e = wait(&mut s, &args(&KEYED, 2)).expect_err("two are stale");
    assert_eq!(
        e.to_string(),
        "after 2 look(s) at the run 2 of 3 artifact(s) have not come: profraw-legacy-1, profraw-js-1"
    );
}

/// Catches: "Re-run failed jobs" broken by a rule on the age of artifacts. The shards that
/// passed are not run again: their jobs are still of attempt 1 and their artifacts older than
/// attempt 2, and they count. Only the shard that failed (no artifact: it never uploaded) is
/// waited for, and it counts once its artifact is made after its job of attempt 2 started.
#[test]
fn rerun_of_the_failed_job_keeps_the_shards_that_passed() {
    let [l1, l2, js] = shard_names();
    let rows = |second: Value| {
        listed(&[
            job(l1, json!("success"), 1, T0),
            second,
            job(js, json!("success"), 1, T0),
            job("test", Value::Null, 2, T2),
        ])
    };
    let mut s = Script::new(vec![
        (
            rows(job(l2, Value::Null, 2, T2)),
            artifacts(&[art("profraw-legacy-1", T1), art("profraw-js-1", T1)]),
        ),
        (
            rows(job(l2, Value::Null, 2, T2)),
            artifacts(&[
                art("profraw-legacy-1", T1),
                art("profraw-js-1", T1),
                art("profraw-legacy-2", T3),
            ]),
        ),
    ]);
    wait(&mut s, &args(&KEYED, 10)).expect("the passed shards count, the rerun one made its own");
    assert_eq!(
        s.pauses, 1,
        "waited for the shard that runs again, not longer"
    );

    // The shard that runs again fails again: the wait ends with it, whatever is there.
    let mut s = Script::new(vec![(
        rows(job(l2, json!("failure"), 2, T2)),
        artifacts(&[art("profraw-legacy-1", T1), art("profraw-js-1", T1)]),
    )]);
    let e = wait(&mut s, &args(&KEYED, 10)).expect_err("failed again");
    assert_eq!(
        e.to_string(),
        "test-shard (legacy-2-of-2) (failure) did not succeed, so its artifact will not come"
    );

    // A passed shard's artifact that has expired is missing, and said so once all have ended.
    let mut s = Script::new(vec![(
        listed(&[
            job(l1, json!("success"), 1, T0),
            job(l2, json!("success"), 2, T2),
            job(js, json!("success"), 1, T0),
        ]),
        artifacts(&[
            json!({ "name": "profraw-legacy-1", "expired": true, "created_at": T1 }),
            art("profraw-legacy-2", T3),
            art("profraw-js-1", T1),
        ]),
    )]);
    let e = wait(&mut s, &args(&KEYED, 10)).expect_err("expired");
    assert_eq!(
        e.to_string(),
        "every job named test-shard* has finished and these artifacts did not come: profraw-legacy-1"
    );
}

/// Catches: an artifact of a job of this attempt counted without proof that it is newer than
/// the job's start: one made at the very start counts, one without a creation time, or a job
/// without a start time, does not; a keyed artifact whose job is not listed does not count (a
/// key that matches nothing would otherwise pass for ever); a job of another shard with the
/// same text in its name is not enough.
#[test]
fn an_artifact_counts_only_with_a_creation_time_after_the_start_of_its_job() {
    let one = |row: Value, artifact: Value| {
        let mut s = Script::new(vec![(listed(&[row]), artifacts(&[artifact]))]);
        wait(&mut s, &args(&["profraw-js-1=(js-1-of-1)"], 1)).is_ok()
    };
    let name = "test-shard (js-1-of-1)";
    assert!(one(job(name, Value::Null, 2, T2), art("profraw-js-1", T2)));
    assert!(!one(job(name, Value::Null, 2, T2), art("profraw-js-1", T1)));
    assert!(!one(
        job(name, Value::Null, 2, T2),
        json!({ "name": "profraw-js-1", "expired": false })
    ));
    assert!(!one(
        json!({ "name": name, "conclusion": null, "run_attempt": 2 }),
        art("profraw-js-1", T3)
    ));
    assert!(!one(
        job("test-shard (js-1-of-10)", Value::Null, 2, T2),
        art("profraw-js-1", T3)
    ));
    assert!(!one(
        job("test-shard (legacy-1-of-2)", Value::Null, 2, T2),
        art("profraw-js-1", T3)
    ));
    // A job of the first attempt, in a run with no later one: its artifact is simply there.
    assert!(one(
        job(name, json!("success"), 1, T0),
        art("profraw-js-1", T1)
    ));

    // An artifact asked for by name alone counts by name, whatever its age.
    let mut s = Script::new(vec![(
        listed(&[job("test-shard (js-1-of-1)", Value::Null, 2, T2)]),
        artifacts(&[art("profraw-js-1", T1)]),
    )]);
    wait(&mut s, &args(&["profraw-js-1"], 1)).expect("by name");
}

/// Catches: the newest attempt taken as attempt 1 or from the shard alone. It is the highest
/// attempt among all listed jobs (`test` is one of them), so a shard whose latest job is of
/// attempt 1 beside a `test` of attempt 2 was not run again and its artifact counts; the same
/// job with no later attempt listed is the newest, and a stale artifact does not count.
#[test]
fn the_newest_attempt_is_the_highest_among_the_listed_jobs() {
    let name = "test-shard (js-1-of-1)";
    // `test` runs in attempt 2 and this shard's latest job is the first attempt's: it counts.
    let mut s = Script::new(vec![(
        listed(&[
            job(name, json!("success"), 1, T0),
            job("test", Value::Null, 2, T2),
        ]),
        artifacts(&[art("profraw-js-1", T1)]),
    )]);
    wait(&mut s, &args(&["profraw-js-1=(js-1-of-1)"], 1)).expect("first attempt's job, passed");

    // The same job in a run with no other attempt is the newest, and a stale artifact (made
    // before the job started) does not count.
    let mut s = Script::new(vec![(
        listed(&[job(name, Value::Null, 1, T2)]),
        artifacts(&[art("profraw-js-1", T1)]),
    )]);
    assert!(wait(&mut s, &args(&["profraw-js-1=(js-1-of-1)"], 1)).is_err());
}
