//! `pipeline-watch` (`xtask::pipeline_watch`): the fail-fast of the push pipeline. The decision
//! and the loop are tested with the job list and the cancel as closures; the binary, with
//! `xtask-fake` as `gh` (its `gh-api.json` is the run's job list, its `gh-calls` the cancels).

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::process::Command;

use kbf_testkit::dir::TestDir;
use serde_json::{Value, json};
use xtask::Error;
use xtask::pipeline_watch::{
    Job, MAX_MISSES, Verdict, cancel_path, cancel_run, jobs_of, jobs_path, verdict, watch,
};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const REPO: &str = "komira-ai/kbf";
const RUN: u64 = 4242;
const LAST: &str = "passed";

fn job(name: &str, status: &str, conclusion: &str) -> Job {
    Job {
        name: name.to_owned(),
        status: status.to_owned(),
        conclusion: conclusion.to_owned(),
    }
}

fn running(name: &str) -> Job {
    job(name, "in_progress", "")
}

fn failed(name: &str) -> Job {
    job(name, "completed", "failure")
}

#[test]
fn a_failed_or_timed_out_job_is_a_failure_and_nothing_else_is() {
    // Catches: a watcher that cancels the run for a job that is only running, queued,
    // skipped, cancelled or green (a cancelled sibling is what the cancel itself leaves),
    // or one that misses a timed-out job, or a job that has a conclusion but is not completed.
    let jobs = [
        running("a"),
        job("b", "queued", ""),
        job("c", "completed", "success"),
        job("d", "completed", "skipped"),
        job("e", "completed", "cancelled"),
        job("f", "in_progress", "failure"),
    ];
    assert_eq!(verdict(&jobs, LAST), Verdict::Running);
    let jobs = [
        running("a"),
        failed("beta / test-x86"),
        job("smoke / smoke", "completed", "timed_out"),
    ];
    assert_eq!(
        verdict(&jobs, LAST),
        Verdict::Failed(vec![
            "beta / test-x86".to_owned(),
            "smoke / smoke".to_owned()
        ])
    );
}

#[test]
fn the_end_of_the_run_wins_over_a_failure_and_only_a_completed_last_job_is_the_end() {
    // Catches: a watcher that keeps running after the closing job has completed (the run
    // would stay open to the end of its polls), one that cancels a run that is over, and one
    // that takes a closing job that is still queued, or a job of another name, for the end.
    let over = [failed("a"), job(LAST, "completed", "skipped")];
    assert_eq!(verdict(&over, LAST), Verdict::Over);
    let not_yet = [
        job(LAST, "queued", ""),
        job("passed-not", "completed", "success"),
    ];
    assert_eq!(verdict(&not_yet, LAST), Verdict::Running);
    assert_eq!(verdict(&[], LAST), Verdict::Running);
}

#[test]
fn the_jobs_are_read_from_the_page_and_a_page_without_jobs_is_refused() {
    // Catches: a name, status or conclusion read from the wrong key; a conclusion GitHub has
    // not set yet (null) read as a failure; an answer that lists no jobs read as no failure.
    let page = json!({"total_count": 2, "jobs": [
        {"name": "ci / test", "status": "completed", "conclusion": "failure"},
        {"name": "watch", "status": "in_progress", "conclusion": null},
    ]});
    assert_eq!(
        jobs_of(&page),
        Ok(vec![failed("ci / test"), running("watch")])
    );
    assert!(matches!(
        jobs_of(&json!({"message": "x"})),
        Err(Error::Failed(_))
    ));
}

#[test]
fn the_paths_name_the_repository_the_run_and_the_latest_attempt() {
    // Catches: a path for another run or repository, a list that includes the attempts a re-run
    // replaced (their failures would cancel the re-run), and a page number lost.
    assert_eq!(
        jobs_path(REPO, RUN, 2),
        "repos/komira-ai/kbf/actions/runs/4242/jobs?per_page=100&filter=latest&page=2"
    );
    assert_eq!(
        cancel_path(REPO, RUN),
        "repos/komira-ai/kbf/actions/runs/4242/cancel"
    );
}

/// Runs `watch` on the successive job lists `reads` (an `Err` is a read that failed; the last
/// read repeats), and returns the lines, how many times it cancelled and how many it paused.
fn drive(
    reads: Vec<Result<Vec<Job>, Error>>,
    max_polls: u32,
    cancel_error: bool,
) -> (Result<Vec<String>, Error>, u32, u32) {
    let at = Cell::new(0);
    let cancels = Cell::new(0);
    let pauses = Cell::new(0);
    let reads = RefCell::new(reads);
    let lines = watch(
        LAST,
        max_polls,
        || {
            let reads = reads.borrow();
            let i = at.get().min(reads.len() - 1);
            at.set(at.get() + 1);
            match &reads[i] {
                Ok(jobs) => Ok(jobs.clone()),
                Err(_) => Err(Error::Failed("unreadable".to_owned())),
            }
        },
        || {
            cancels.set(cancels.get() + 1);
            if cancel_error {
                Err(Error::Failed("refused".to_owned()))
            } else {
                Ok(())
            }
        },
        || pauses.set(pauses.get() + 1),
    );
    (lines, cancels.get(), pauses.get())
}

fn unreadable() -> Result<Vec<Job>, Error> {
    Err(Error::Failed("unreadable".to_owned()))
}

#[test]
fn a_failure_cancels_the_run_once_and_names_the_job() {
    // Catches: a watcher that sees a failure and does not cancel, cancels twice, or says
    // nothing of which job failed (the log of the watch is where a reader looks first).
    let (lines, cancels, pauses) = drive(
        vec![
            Ok(vec![running("a")]),
            Ok(vec![running("a"), failed("beta / native-macos")]),
        ],
        50,
        false,
    );
    let lines = lines.expect("watched");
    assert_eq!(cancels, 1);
    assert_eq!(
        pauses, 1,
        "one pause, between the first read and the second"
    );
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("beta / native-macos"), "{lines:?}");
}

#[test]
fn a_cancel_that_is_refused_is_an_error() {
    // Catches: a watcher that reports a cancel GitHub refused as done: the jobs would run on
    // and nothing in the log would say why.
    let (lines, cancels, _) = drive(vec![Ok(vec![failed("a")])], 50, true);
    assert_eq!(cancels, 1);
    assert_eq!(lines, Err(Error::Failed("refused".to_owned())));
}

#[test]
fn a_run_that_ends_without_a_failure_is_not_cancelled() {
    // Catches: a cancel of a run that passed, and a watch that outlives the closing job.
    let (lines, cancels, pauses) = drive(
        vec![
            Ok(vec![running("a")]),
            Ok(vec![job("a", "completed", "success"), running("b")]),
            Ok(vec![job(LAST, "completed", "success")]),
        ],
        50,
        false,
    );
    assert!(lines.expect("watched")[0].contains("is over"));
    assert_eq!((cancels, pauses), (0, 2));
}

#[test]
fn the_watch_ends_after_its_polls_without_cancelling_or_failing() {
    // Catches: a watch with no end (a run held open for hours by a closing job it never
    // sees), and one that turns the end of its polls into a failure of the run.
    let (lines, cancels, pauses) = drive(vec![Ok(vec![running("a")])], 3, false);
    assert!(lines.expect("watched")[0].contains("3 reads"));
    assert_eq!((cancels, pauses), (0, 3));
    let (lines, cancels, pauses) = drive(vec![Ok(vec![])], 0, false);
    assert!(lines.is_ok());
    assert_eq!((cancels, pauses), (0, 0));
}

#[test]
fn a_job_list_that_cannot_be_read_ends_the_watch_without_failing_the_run() {
    // Catches: a flaky read of the API that turns the watcher red (so the pipeline red), one
    // that gives up on the first miss, and one that never gives up.
    let (lines, cancels, pauses) = drive(vec![unreadable()], 100, false);
    assert!(lines.expect("watched")[0].contains("without fail-fast"));
    assert_eq!(cancels, 0);
    assert_eq!(pauses, MAX_MISSES - 1);
}

#[test]
fn a_read_that_works_forgets_the_misses_before_it() {
    // Catches: misses counted over the whole watch instead of in a row, which would end the
    // watch of a long run on its tenth hiccup, hours apart.
    let mut reads: Vec<Result<Vec<Job>, Error>> = Vec::new();
    for _ in 0..3 {
        for _ in 0..MAX_MISSES - 1 {
            reads.push(unreadable());
        }
        reads.push(Ok(vec![running("a")]));
    }
    reads.push(Ok(vec![failed("a")]));
    let (lines, cancels, _) = drive(reads, 1000, false);
    assert!(lines.expect("watched")[0].contains("failed: a"));
    assert_eq!(cancels, 1);
}

/// An empty directory of its own for one test, holding the fake as `gh/gh`.
fn scratch(name: &str) -> TestDir {
    let dir = TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-pipeline-watch"),
        name,
    );
    std::fs::create_dir_all(dir.join("gh")).expect("scratch");
    std::fs::hard_link(FAKE, dir.join("gh/gh")).expect("link the fake as gh");
    dir
}

/// Writes GitHub's answers, `(path, JSON)`, to the fake's `gh-api.json`.
fn answers(root: &Path, answers: &[(String, Value)]) {
    let map: serde_json::Map<String, Value> = answers.iter().cloned().collect();
    std::fs::write(root.join("gh/gh-api.json"), Value::Object(map).to_string())
        .expect("gh-api.json");
}

fn page(jobs: &[(&str, &str, Value)]) -> Value {
    json!({"total_count": jobs.len(), "jobs": jobs.iter().map(|(name, status, conclusion)| {
        json!({"name": name, "status": status, "conclusion": conclusion})
    }).collect::<Vec<_>>()})
}

/// `cargo xtask pipeline-watch` with the fake as `gh`: the exit code and the lines on stdout and
/// stderr. At most `polls` reads, no wait between them.
fn xtask(root: &Path, polls: &str) -> (Option<i32>, String, String) {
    let out = Command::new(XTASK)
        .args(["pipeline-watch", "--repo", REPO, "--run-id", "4242"])
        .args([
            "--last-job",
            LAST,
            "--interval-secs",
            "0",
            "--max-polls",
            polls,
        ])
        .arg("--gh")
        .arg(root.join("gh/gh"))
        .env_remove("XTASK_FAKE_DEFECT")
        .output()
        .expect("run xtask");
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    (out.status.code(), text(&out.stdout), text(&out.stderr))
}

fn calls(root: &Path) -> String {
    std::fs::read_to_string(root.join("gh/gh-calls")).unwrap_or_default()
}

#[test]
fn the_binary_cancels_the_run_when_a_job_has_failed() {
    // Catches: a cancel sent to another path or with another method, a job list read from
    // the first page only (the failure here is on the second), and a failure that does not
    // end the watch with a line saying which job failed.
    let root = scratch("fails");
    let first: Vec<(String, String, Value)> = (0..100)
        .map(|i| (format!("j{i}"), "completed".to_owned(), json!("success")))
        .collect();
    let first: Vec<(&str, &str, Value)> = first
        .iter()
        .map(|(n, s, c)| (n.as_str(), s.as_str(), c.clone()))
        .collect();
    answers(
        &root,
        &[
            (jobs_path(REPO, RUN, 1), page(&first)),
            (
                jobs_path(REPO, RUN, 2),
                page(&[("beta / test-arm", "completed", json!("failure"))]),
            ),
        ],
    );
    let (code, out, err) = xtask(&root, "5");
    assert_eq!(code, Some(0), "{err}");
    assert!(out.contains("failed: beta / test-arm"), "{out}");
    let calls = calls(&root);
    assert_eq!(calls.lines().count(), 1, "{calls}");
    let call: Value = serde_json::from_str(calls.trim()).expect("a call");
    assert_eq!(
        call["args"],
        json!(["api", "-X", "POST", cancel_path(REPO, RUN)])
    );
}

#[test]
fn the_binary_does_not_cancel_a_run_that_is_over_or_still_going() {
    // Catches: a cancel without a failure, and a watch that does not end at the closing job.
    let root = scratch("over");
    answers(
        &root,
        &[(
            jobs_path(REPO, RUN, 1),
            page(&[
                ("a", "completed", json!("skipped")),
                (LAST, "completed", json!("skipped")),
            ]),
        )],
    );
    let (code, out, _) = xtask(&root, "5");
    assert_eq!(code, Some(0));
    assert!(out.contains("is over"), "{out}");
    assert_eq!(calls(&root), "");
    let root = scratch("going");
    answers(
        &root,
        &[(
            jobs_path(REPO, RUN, 1),
            page(&[("a", "in_progress", Value::Null)]),
        )],
    );
    let (code, out, _) = xtask(&root, "2");
    assert_eq!(code, Some(0));
    assert!(out.contains("2 reads"), "{out}");
    assert_eq!(calls(&root), "");
}

#[test]
fn the_binary_exits_zero_when_the_api_answers_nothing_it_can_use() {
    // Catches: an unreadable job list (the run not found, an answer without jobs, a `gh` that
    // is gone) turning the watcher, and so the pipeline, red.
    let root = scratch("blind");
    let (code, out, err) = xtask(&root, "2");
    assert_eq!((code, calls(&root).as_str()), (Some(0), ""), "{err}");
    assert!(out.contains("2 reads"), "{out}");
    answers(&root, &[(jobs_path(REPO, RUN, 1), json!({"message": "x"}))]);
    assert_eq!(xtask(&root, "2").0, Some(0));
    let out = Command::new(XTASK)
        .args(["pipeline-watch", "--repo", REPO, "--run-id", "4242"])
        .args([
            "--last-job",
            LAST,
            "--interval-secs",
            "0",
            "--max-polls",
            "2",
        ])
        .arg("--gh")
        .arg(root.join("no-such-gh"))
        .output()
        .expect("run xtask");
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn a_gh_that_cannot_be_started_is_an_error_naming_it() {
    // Catches: a cancel that reports success when `gh` was never run, so the jobs go on and
    // nothing says why.
    let said = cancel_run(Path::new("/nonexistent/gh"), REPO, RUN);
    assert!(matches!(said, Err(Error::Spawn { .. })), "{said:?}");
}

#[test]
fn the_binary_fails_only_when_github_refuses_the_cancel_and_not_when_it_is_already_over() {
    // Catches: a refused cancel reported as done, and a run that finished a moment before the
    // cancel (HTTP 409) reported as a failure of the watcher.
    let root = scratch("refused");
    answers(
        &root,
        &[(
            jobs_path(REPO, RUN, 1),
            page(&[("a", "completed", json!("failure"))]),
        )],
    );
    std::fs::write(root.join("gh/gh-fault"), "fail-write").expect("fault");
    let (code, _, err) = xtask(&root, "2");
    assert_eq!(code, Some(1));
    assert!(err.contains("HTTP 404"), "{err}");
    std::fs::write(root.join("gh/gh-fault"), "conflict").expect("fault");
    let (code, out, _) = xtask(&root, "2");
    assert_eq!(code, Some(0));
    assert!(out.contains("failed: a"), "{out}");
}
