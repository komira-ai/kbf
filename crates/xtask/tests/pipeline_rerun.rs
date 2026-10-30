//! `pipeline-rerun` (`xtask::pipeline_rerun`): the one rerun of a completed pipeline run whose
//! failed jobs all build or move artifacts. The decision is tested on explicit run and job
//! values; the binary, with `xtask-fake` as `gh` (its `gh-api.json` is the run and its job list,
//! its `gh-calls` the rerun). Nothing here waits or depends on a clock.

use std::path::Path;
use std::process::Command;

use kbf_testkit::dir::TestDir;
use serde_json::{Value, json};
use xtask::pipeline_rerun::{
    Decision, JobInfo, RERUNNABLE, RunInfo, decide, jobs_of, jobs_path, rerun_path, rerunnable,
    run_of, run_path, summary_line,
};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const REPO: &str = "komira-ai/kbf";
const RUN: u64 = 4242;
const SUMS: &str = "artifacts / sums";
const BUILD: &str = "artifacts / linux-x86_64";

fn url_of(name: &str) -> String {
    format!(
        "https://github.com/{REPO}/actions/runs/{RUN}/job/{}",
        name.len()
    )
}

fn job(name: &str, conclusion: &str) -> JobInfo {
    JobInfo {
        name: name.to_owned(),
        status: "completed".to_owned(),
        conclusion: conclusion.to_owned(),
        url: url_of(name),
    }
}

fn failed(name: &str) -> JobInfo {
    job(name, "failure")
}

fn run_info(attempt: u64, conclusion: &str) -> RunInfo {
    RunInfo {
        attempt,
        conclusion: conclusion.to_owned(),
        branch: "main".to_owned(),
        event: "push".to_owned(),
    }
}

fn nothing(decision: &Decision) -> &str {
    match decision {
        Decision::Nothing(why) => why,
        Decision::Rerun(jobs) => panic!("a rerun of {jobs:?} was not expected"),
    }
}

#[test]
fn only_the_jobs_that_build_or_move_artifacts_are_rerunnable_by_exact_name() {
    // Catches: a rerun for a test or lint job (a flaky test would be hidden, not fixed), for a
    // job nobody listed, for a name that only contains or starts like a listed one, and a
    // listed job missing from the list.
    for name in [
        "artifacts / linux-x86_64",
        "artifacts / linux-arm64",
        "artifacts / darwin-arm64",
        "artifacts / sums-linux",
        "artifacts / sums-darwin-arm64",
        "artifacts / sums",
        "artifacts-sign / attest-linux",
        "artifacts-sign / attest-darwin-arm64",
        "artifacts-sign / verify-linux",
        "artifacts-sign / verify-darwin-arm64",
    ] {
        assert!(rerunnable(name), "{name}");
        assert!(RERUNNABLE.contains(&name), "{name}");
    }
    assert_eq!(RERUNNABLE.len(), 10);
    for name in [
        "ci / test",
        "ci / lint",
        "beta / test-x86",
        "beta / native-macos",
        "smoke / smoke",
        "watch",
        "passed",
        "artifacts / new-job",
        "artifacts / sums-linux-extra",
        "artifacts / linux-x86_64 (retry)",
        "artifacts-sign / sums",
        "sums",
        "Artifacts / sums",
        "",
    ] {
        assert!(!rerunnable(name), "{name}");
    }
}

#[test]
fn a_first_attempt_whose_failed_jobs_all_build_or_move_artifacts_is_rerun() {
    // Catches: no rerun for the flake this exists for, for a run that ended cancelled (the
    // watch cancels a run for a failed job, so that is how the failed pipeline ends), for a
    // timed-out job, and a rerun of a list that leaves a failed artifact job out.
    let jobs = [
        failed(SUMS),
        job("artifacts / linux-arm64", "success"),
        job("ci / test", "cancelled"),
        job("smoke / smoke", "skipped"),
        job("artifacts / darwin-arm64", "timed_out"),
    ];
    for conclusion in ["failure", "cancelled"] {
        assert_eq!(
            decide(&run_info(1, conclusion), &jobs),
            Decision::Rerun(vec![
                failed(SUMS),
                job("artifacts / darwin-arm64", "timed_out")
            ]),
            "{conclusion}"
        );
    }
}

#[test]
fn the_second_attempt_does_nothing() {
    // Catches: a rerun loop (the completion of the rerun is attempt 2 and would start another).
    let why = decide(&run_info(2, "failure"), &[failed(SUMS)]);
    assert!(nothing(&why).contains("attempt 2"), "{why:?}");
    assert!(matches!(
        decide(&run_info(3, "failure"), &[failed(SUMS)]),
        Decision::Nothing(_)
    ));
}

#[test]
fn a_test_or_lint_job_beside_an_artifact_failure_does_nothing() {
    // Catches: a rerun that hides a failed test because an artifact job failed too, and one
    // that names the jobs that make it refuse.
    for other in ["ci / test", "ci / lint", "beta / test-arm", "smoke / smoke"] {
        let decision = decide(&run_info(1, "failure"), &[failed(SUMS), failed(other)]);
        assert!(nothing(&decision).contains(other), "{decision:?}");
        assert!(!nothing(&decision).contains(SUMS), "{decision:?}");
    }
}

#[test]
fn an_unknown_job_does_nothing() {
    // Catches: a job nobody listed (a new one, a renamed one) rerun by default.
    let decision = decide(&run_info(1, "failure"), &[failed("artifacts / brand-new")]);
    assert!(nothing(&decision).contains("brand-new"), "{decision:?}");
    let decision = decide(&run_info(1, "failure"), &[failed(BUILD), failed("x / y")]);
    assert!(matches!(decision, Decision::Nothing(_)));
}

#[test]
fn a_run_that_did_not_fail_or_is_not_a_push_to_main_does_nothing() {
    // Catches: a rerun of a green run, of a run nobody failed (cancelled by hand, or cancelled
    // with no failed job), of a skipped one, and of a run of another event or branch.
    for conclusion in ["success", "skipped", "action_required", ""] {
        assert!(
            matches!(
                decide(&run_info(1, conclusion), &[failed(SUMS)]),
                Decision::Nothing(_)
            ),
            "{conclusion}"
        );
    }
    let jobs = [job(SUMS, "success"), job("ci / test", "cancelled")];
    for conclusion in ["failure", "cancelled"] {
        let decision = decide(&run_info(1, conclusion), &jobs);
        assert!(nothing(&decision).contains("no job failed"), "{decision:?}");
    }
    assert!(matches!(
        decide(&run_info(1, "failure"), &[]),
        Decision::Nothing(_)
    ));
    for (branch, event) in [("feature", "push"), ("main", "pull_request"), ("main", "")] {
        let run = RunInfo {
            branch: branch.to_owned(),
            event: event.to_owned(),
            ..run_info(1, "failure")
        };
        assert!(
            matches!(decide(&run, &[failed(SUMS)]), Decision::Nothing(_)),
            "{branch} {event}"
        );
    }
    // A job that has a conclusion but is not completed is not a failure.
    let mut running = failed(SUMS);
    running.status = "in_progress".to_owned();
    assert!(matches!(
        decide(&run_info(1, "failure"), &[running]),
        Decision::Nothing(_)
    ));
}

#[test]
fn the_summary_line_names_the_jobs_and_links_their_first_failure() {
    // Catches: a rerun that leaves no trace (flakes would not be countable), a line without a
    // job's name or its log link, and one that drops the second job.
    let line = summary_line(&[failed(SUMS), failed(BUILD)]);
    for want in [SUMS, BUILD, &url_of(SUMS), &url_of(BUILD)] {
        assert!(line.contains(want), "{want} not in {line}");
    }
    assert!(!line.contains('\n'), "{line}");
}

#[test]
fn the_answers_are_read_by_key_and_what_lacks_them_is_refused() {
    // Catches: an attempt, conclusion, branch, event, name, status, conclusion or link read from
    // the wrong key; a run with no attempt read as attempt 0 or 1 (it would be rerun); an
    // answer without jobs read as no failure; and a wrong path (run, repository, page).
    let run = json!({"run_attempt": 2, "conclusion": "failure", "head_branch": "main",
        "event": "push"});
    assert_eq!(
        run_of(&run),
        Ok(RunInfo {
            attempt: 2,
            ..run_info(2, "failure")
        })
    );
    assert!(run_of(&json!({"conclusion": "failure"})).is_err());
    let page = json!({"jobs": [
        {"name": SUMS, "status": "completed", "conclusion": "failure", "html_url": "https://x/1"},
        {"name": "watch", "status": "in_progress", "conclusion": null},
    ]});
    let mut sums = failed(SUMS);
    sums.url = "https://x/1".to_owned();
    let mut watch = job("watch", "");
    watch.status = "in_progress".to_owned();
    watch.url = String::new();
    assert_eq!(jobs_of(&page), Ok(vec![sums, watch]));
    assert!(jobs_of(&json!({"message": "x"})).is_err());
    assert_eq!(run_path(REPO, RUN), "repos/komira-ai/kbf/actions/runs/4242");
    assert_eq!(
        rerun_path(REPO, RUN),
        "repos/komira-ai/kbf/actions/runs/4242/rerun-failed-jobs"
    );
    assert_eq!(
        jobs_path(REPO, RUN, 2),
        "repos/komira-ai/kbf/actions/runs/4242/jobs?per_page=100&filter=latest&page=2"
    );
}

/// An empty directory of its own for one test, holding the fake as `gh/gh`.
fn scratch(name: &str) -> TestDir {
    let dir = TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-pipeline-rerun"),
        name,
    );
    std::fs::create_dir_all(dir.join("gh")).expect("scratch");
    std::fs::hard_link(FAKE, dir.join("gh/gh")).expect("link the fake as gh");
    dir
}

/// Writes the run and its jobs as GitHub answers them to the fake's `gh-api.json`.
fn github(root: &Path, attempt: u64, conclusion: &str, jobs: &[(&str, &str)]) {
    let jobs: Vec<Value> = jobs
        .iter()
        .map(|(name, conclusion)| {
            json!({"name": name, "status": "completed", "conclusion": conclusion,
                "html_url": url_of(name)})
        })
        .collect();
    let answers = json!({
        run_path(REPO, RUN): {"run_attempt": attempt, "conclusion": conclusion,
            "head_branch": "main", "event": "push"},
        jobs_path(REPO, RUN, 1): {"total_count": jobs.len(), "jobs": jobs},
    });
    std::fs::write(root.join("gh/gh-api.json"), answers.to_string()).expect("gh-api.json");
}

/// `cargo xtask pipeline-rerun` with the fake as `gh`: the exit code, stdout and stderr.
fn xtask(root: &Path, summary: Option<&Path>) -> (Option<i32>, String, String) {
    let mut cmd = Command::new(XTASK);
    cmd.args(["pipeline-rerun", "--repo", REPO, "--run-id", "4242"])
        .arg("--gh")
        .arg(root.join("gh/gh"))
        .env_remove("GITHUB_STEP_SUMMARY")
        .env_remove("XTASK_FAKE_DEFECT");
    if let Some(file) = summary {
        cmd.env("GITHUB_STEP_SUMMARY", file);
    }
    let out = cmd.output().expect("run xtask");
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    (out.status.code(), text(&out.stdout), text(&out.stderr))
}

fn calls(root: &Path) -> String {
    std::fs::read_to_string(root.join("gh/gh-calls")).unwrap_or_default()
}

#[test]
fn the_binary_reruns_the_failed_jobs_once_and_writes_the_summary() {
    // Catches: a rerun sent to another path or method, none for an all-artifact failure, and a
    // summary file that does not get the jobs and the links of their first failure.
    let root = scratch("rerun");
    github(
        &root,
        1,
        "cancelled",
        &[
            (SUMS, "failure"),
            (BUILD, "failure"),
            ("ci / test", "cancelled"),
        ],
    );
    let summary = root.join("summary.md");
    let (code, out, err) = xtask(&root, Some(&summary));
    assert_eq!(code, Some(0), "{err}");
    let calls = calls(&root);
    assert_eq!(calls.lines().count(), 1, "{calls}");
    let call: Value = serde_json::from_str(calls.trim()).expect("a call");
    assert_eq!(
        call["args"],
        json!(["api", "-X", "POST", rerun_path(REPO, RUN)])
    );
    let text = std::fs::read_to_string(&summary).expect("the summary");
    for want in [SUMS, BUILD, &url_of(SUMS), &url_of(BUILD)] {
        assert!(text.contains(want), "{want} not in {text}");
    }
    assert!(out.contains(SUMS), "{out}");
}

#[test]
fn the_binary_does_nothing_for_attempt_two_a_test_failure_an_unknown_job_or_a_success() {
    // Catches: a call to GitHub for any of these, a summary written for a rerun that was not
    // made, and an exit code other than 0 for a run that needs nothing.
    type Case<'a> = (&'a str, u64, &'a str, Vec<(&'a str, &'a str)>);
    let cases: [Case; 4] = [
        ("attempt-2", 2, "failure", vec![(SUMS, "failure")]),
        (
            "test-beside",
            1,
            "failure",
            vec![(SUMS, "failure"), ("ci / test", "failure")],
        ),
        (
            "unknown",
            1,
            "failure",
            vec![("artifacts / new", "failure")],
        ),
        ("success", 1, "success", vec![(SUMS, "success")]),
    ];
    for (name, attempt, conclusion, jobs) in cases {
        let root = scratch(name);
        github(&root, attempt, conclusion, &jobs);
        let summary = root.join("summary.md");
        let (code, out, err) = xtask(&root, Some(&summary));
        assert_eq!(code, Some(0), "{name}: {err}");
        assert!(out.contains("nothing to rerun"), "{name}: {out}");
        assert_eq!(calls(&root), "", "{name}");
        assert!(!summary.exists(), "{name}");
    }
}

#[test]
fn a_rerun_that_github_refuses_is_reported_and_exits_non_zero() {
    // Catches: a refused rerun (HTTP 403) reported as done, so a pipeline that stays red for a
    // flake would look as if it was being retried, with no summary line either.
    let root = scratch("refused");
    github(&root, 1, "failure", &[(SUMS, "failure")]);
    std::fs::write(root.join("gh/gh-fault"), "refuse-rerun").expect("fault");
    let summary = root.join("summary.md");
    let (code, _, err) = xtask(&root, Some(&summary));
    assert_eq!(code, Some(1), "{err}");
    assert!(
        err.contains("HTTP 403") && err.contains("rerun-failed-jobs"),
        "{err}"
    );
    assert!(!summary.exists());
}

#[test]
fn the_binary_fails_when_the_run_cannot_be_read() {
    // Catches: a run GitHub does not list read as nothing to do (a rerun that silently never
    // happens), whatever the reason.
    let root = scratch("unreadable");
    let (code, _, err) = xtask(&root, None);
    assert_eq!(code, Some(1), "{err}");
    assert_eq!(calls(&root), "");
}

#[test]
fn the_binary_reads_every_page_of_jobs_and_reruns_for_a_failure_on_a_later_page() {
    // Catches: a job list read from the first page only (a failed artifact job on page 2 would
    // be missed and, worse, a failed test job on page 2 would be missed beside an artifact one).
    let root = scratch("pages");
    let first: Vec<Value> = (0..100)
        .map(|i| {
            json!({"name": format!("j{i}"), "status": "completed", "conclusion": "success",
            "html_url": "https://x"})
        })
        .collect();
    let second = json!([{"name": SUMS, "status": "completed", "conclusion": "failure",
        "html_url": url_of(SUMS)}]);
    let answers = json!({
        run_path(REPO, RUN): {"run_attempt": 1, "conclusion": "failure",
            "head_branch": "main", "event": "push"},
        jobs_path(REPO, RUN, 1): {"total_count": 101, "jobs": first},
        jobs_path(REPO, RUN, 2): {"total_count": 101, "jobs": second},
    });
    std::fs::write(root.join("gh/gh-api.json"), answers.to_string()).expect("gh-api.json");
    let (code, _, err) = xtask(&root, None);
    assert_eq!(code, Some(0), "{err}");
    assert!(calls(&root).contains("rerun-failed-jobs"));
    // The same, with a failed test job on page 2: nothing is rerun.
    let second = json!([{"name": "ci / test", "status": "completed", "conclusion": "failure",
        "html_url": "https://x"}]);
    let full: Vec<Value> = (0..100)
        .map(|_| {
            json!({"name": SUMS, "status": "completed", "conclusion": "failure",
            "html_url": "https://x"})
        })
        .collect();
    let answers = json!({
        run_path(REPO, RUN): {"run_attempt": 1, "conclusion": "failure",
            "head_branch": "main", "event": "push"},
        jobs_path(REPO, RUN, 1): {"total_count": 101, "jobs": full},
        jobs_path(REPO, RUN, 2): {"total_count": 101, "jobs": second},
    });
    let other = scratch("pages-test");
    std::fs::write(other.join("gh/gh-api.json"), answers.to_string()).expect("gh-api.json");
    let (code, out, err) = xtask(&other, None);
    assert_eq!(code, Some(0), "{err}");
    assert!(out.contains("ci / test"), "{out}");
    assert_eq!(calls(&other), "");
}

#[test]
fn the_binary_fails_when_the_jobs_of_the_run_cannot_be_read() {
    // Catches: a run that is listed but whose jobs are not (404) read as "no job failed", so a
    // rerun that was wanted silently never happens.
    let root = scratch("no-jobs");
    let answers = json!({run_path(REPO, RUN): {"run_attempt": 1, "conclusion": "failure",
        "head_branch": "main", "event": "push"}});
    std::fs::write(root.join("gh/gh-api.json"), answers.to_string()).expect("gh-api.json");
    let (code, _, err) = xtask(&root, None);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("not found"), "{err}");
    assert_eq!(calls(&root), "");
}

#[test]
fn the_rerun_is_made_and_printed_when_there_is_no_summary_file() {
    // Catches: a run outside Actions (no GITHUB_STEP_SUMMARY) that fails or skips the rerun
    // because it has nowhere to write the summary.
    let root = scratch("no-summary");
    github(&root, 1, "failure", &[(BUILD, "failure")]);
    let (code, out, err) = xtask(&root, None);
    assert_eq!(code, Some(0), "{err}");
    assert!(out.contains(BUILD) && out.contains(&url_of(BUILD)), "{out}");
    assert_eq!(calls(&root).lines().count(), 1);
}

#[test]
fn a_summary_file_that_cannot_be_written_is_an_error_after_the_rerun() {
    // Catches: flakes that go uncounted without a trace: a summary that cannot be written must
    // fail the run (the rerun itself has been made, so the call is there).
    let root = scratch("bad-summary");
    github(&root, 1, "failure", &[(SUMS, "failure")]);
    let (code, _, err) = xtask(&root, Some(&root.join("no-such-dir/summary.md")));
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("append to"), "{err}");
    assert_eq!(calls(&root).lines().count(), 1);
}

#[test]
fn the_fake_refuses_only_the_rerun_and_a_bodyless_write_elsewhere() {
    // Catches: a fake whose refuse-rerun fault also refuses the cancel (the watch tests would
    // then fail for the wrong reason), and a fake that accepts a bodyless write to a path that
    // needs a body, as GitHub refuses it.
    let root = scratch("fake");
    std::fs::write(root.join("gh/gh-fault"), "refuse-rerun").expect("fault");
    let post = |path: &str| {
        Command::new(root.join("gh/gh"))
            .args(["api", "-X", "POST", path])
            .env_remove("XTASK_FAKE_DEFECT")
            .output()
            .expect("run the fake")
    };
    assert!(
        post(&format!("{}/cancel", run_path(REPO, RUN)))
            .status
            .success()
    );
    let out = post("repos/komira-ai/kbf/issues");
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("HTTP 422"));
}
