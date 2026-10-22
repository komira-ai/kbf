//! `cargo xtask release gates` as the release workflow runs it, against `xtask-fake` as
//! `gh`: its `gh-api.json` holds what GitHub lists as the runs of each workflow, and
//! `gh-args` what was asked, so a test sees that the verb only ever reads.
//!
//! Every red case is run once per workflow (`ci`, `beta`, `artifacts`) with the other two
//! green, so no workflow is the one that is not looked at.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use kbf_testkit::dir::TestDir;
use serde_json::{Map, Value, json};
use xtask::release::gates::GATES;

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const REPO: &str = "komira-ai/kbf";
const SHA: &str = "357494ac6b0fecfa065d6c8fa038cfca9ae9c13f";
const OTHER: &str = "0123456789abcdef0123456789abcdef01234567";
const BRANCH: &str = "main";

type Ran = (Option<i32>, String, String);

struct Fx {
    root: PathBuf,
    /// Last field: the directory is removed with the fixture.
    _own: TestDir,
}

fn fx(name: &str) -> Fx {
    let own = TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-release-gates"),
        name,
    );
    let root = own.to_path_buf();
    std::fs::create_dir_all(root.join("gh")).expect("gh dir");
    std::fs::hard_link(FAKE, root.join("gh/gh")).expect("link the fake as gh");
    Fx { root, _own: own }
}

fn runs_path(file: &str) -> String {
    format!(
        "repos/{REPO}/actions/workflows/{file}/runs?head_sha={SHA}&event=push\
         &branch={BRANCH}&per_page=100"
    )
}

/// A run of the workflow `file`, as GitHub lists it: the good one, to be spoiled.
fn run(file: &str, id: u64) -> Value {
    json!({
        "id": id,
        "name": file.trim_end_matches(".yml"),
        "path": format!(".github/workflows/{file}"),
        "head_sha": SHA,
        "head_branch": BRANCH,
        "event": "push",
        "status": "completed",
        "conclusion": "success",
        "run_number": 1,
        "run_attempt": 1,
    })
}

/// `run` with `key` set to `value`.
fn with(mut run: Value, key: &str, value: Value) -> Value {
    run[key] = value;
    run
}

/// The ids of the good runs: `ci` 11, `beta` 22, `artifacts` 33.
fn id_of(file: &str) -> u64 {
    match file {
        "ci.yml" => 11,
        "beta.yml" => 22,
        _ => 33,
    }
}

impl Fx {
    fn put(&self, entries: Vec<(String, Value)>) {
        let text = std::fs::read_to_string(self.root.join("gh/gh-api.json")).unwrap_or_default();
        let mut map: Map<String, Value> = serde_json::from_str(&text).unwrap_or_default();
        map.extend(entries);
        std::fs::write(
            self.root.join("gh/gh-api.json"),
            Value::Object(map).to_string(),
        )
        .expect("gh-api.json");
    }

    /// GitHub lists exactly `runs` for the workflow `file`.
    fn lists(&self, file: &str, runs: Vec<Value>) {
        self.put(vec![(runs_path(file), json!({ "workflow_runs": runs }))]);
    }

    /// Every workflow has its good run, except that `file` lists `runs` instead.
    fn green_but(&self, file: &str, runs: Vec<Value>) {
        for (_, f) in GATES {
            self.lists(f, vec![run(f, id_of(f))]);
        }
        self.lists(file, runs);
    }

    /// `release gates` with the fixture's flags, `overrides` replacing the flags they name
    /// and any other appended.
    fn gates_with(&self, overrides: &[(&str, &str)]) -> Ran {
        let gh = self.root.join("gh/gh");
        let mut flags: Vec<(&str, &str)> = vec![
            ("--repo", REPO),
            ("--sha", SHA),
            ("--branch", BRANCH),
            ("--timeout-s", "0"),
            ("--poll-s", "1"),
            ("--gh", gh.to_str().unwrap()),
        ];
        for (flag, value) in overrides {
            match flags.iter_mut().find(|(f, _)| f == flag) {
                Some(found) => found.1 = value,
                None => flags.push((flag, value)),
            }
        }
        let mut cmd = Command::new(XTASK);
        cmd.args(["release", "gates"]);
        for (flag, value) in flags {
            cmd.arg(format!("{flag}={value}"));
        }
        let out = cmd
            .env_remove("XTASK_FAKE_DEFECT")
            .output()
            .expect("run xtask");
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        (out.status.code(), text(&out.stdout), text(&out.stderr))
    }

    fn gates(&self) -> Ran {
        self.gates_with(&[])
    }

    /// The branch head is `head`, and comparing `SHA` with it says `status`.
    fn main_is_at(&self, head: &str, status: &str) {
        self.put(vec![
            (
                format!("repos/{REPO}/branches/{BRANCH}"),
                json!({ "commit": { "sha": head } }),
            ),
            (
                format!("repos/{REPO}/compare/{SHA}...{head}"),
                json!({ "status": status }),
            ),
        ]);
    }

    /// `file`'s run concluded `conclusion`, the others are green.
    fn concluded(&self, file: &str, conclusion: &str) {
        self.green_but(
            file,
            vec![with(
                run(file, id_of(file)),
                "conclusion",
                json!(conclusion),
            )],
        );
    }

    fn gh_args(&self) -> String {
        std::fs::read_to_string(self.root.join("gh/gh-args")).unwrap_or_default()
    }
}

/// `file` listing `runs` while the others are green is red, and says which file.
fn assert_red_for_every_workflow(name: &str, runs: impl Fn(&str) -> Vec<Value>) {
    for (_, file) in GATES {
        let fx = fx(&format!("{name}-{}", file.trim_end_matches(".yml")));
        fx.green_but(file, runs(file));
        let (code, out, err) = fx.gates();
        assert_eq!(code, Some(1), "{name} {file}: {out}{err}");
        assert!(err.contains(&format!("{file}:")), "{name} {file}: {err}");
        assert!(out.is_empty(), "no ids for a red result: {out}");
    }
}

#[test]
fn three_green_push_runs_print_their_ids_and_append_them_to_the_output() {
    // Catches: a wrong or swapped id, or GITHUB_OUTPUT never written, so verdict-write
    // names runs that are not the ones that passed.
    let fx = fx("green");
    fx.green_but("ci.yml", vec![run("ci.yml", 11)]);
    let file = fx.root.join("out");
    let (code, out, err) = fx.gates_with(&[("--output", file.to_str().unwrap())]);
    assert_eq!(code, Some(0), "{err}");
    let want = "ci_run=11\nbeta_run=22\nartifacts_run=33\nsuperseded=false\n";
    assert_eq!(out, want);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), want);
}

#[test]
fn it_only_reads_three_lists_a_look() {
    // Catches: a write (-X, --input) or a call beyond the three lists of one look.
    let fx = fx("reads");
    fx.green_but("ci.yml", vec![run("ci.yml", 11)]);
    assert_eq!(fx.gates().0, Some(0));
    let args = fx.gh_args();
    let lines: Vec<&str> = args.lines().collect();
    assert_eq!(lines.len(), 3, "{args}");
    for (line, (_, file)) in lines.iter().zip(GATES) {
        assert_eq!(*line, format!("api {}", runs_path(file)));
    }
    assert!(
        !args.contains(" -X ") && !args.contains("--input"),
        "{args}"
    );
}

#[test]
fn a_run_that_did_not_succeed_is_red_whatever_the_conclusion() {
    // Catches: cancelled, skipped, neutral or a failure read as good; a conclusion that
    // is missing on a completed run read as good.
    for conclusion in [
        json!("failure"),
        json!("cancelled"),
        json!("skipped"),
        json!("neutral"),
        json!("timed_out"),
        json!("action_required"),
        Value::Null,
    ] {
        assert_red_for_every_workflow(&format!("conclusion-{conclusion}"), |f| {
            vec![with(run(f, id_of(f)), "conclusion", conclusion.clone())]
        });
    }
}

#[test]
fn the_message_names_the_run_and_its_conclusion() {
    let fx = fx("message");
    fx.green_but(
        "beta.yml",
        vec![with(run("beta.yml", 22), "conclusion", json!("cancelled"))],
    );
    let (_, _, err) = fx.gates();
    assert!(err.contains("beta.yml: run 22"), "{err}");
    assert!(err.contains("concluded cancelled, not success"), "{err}");
}

#[test]
fn a_workflow_that_did_not_run_is_red() {
    // Catches: "not run" read as pass (beta's pending run is replaced, so a commit may
    // have no beta run at all), whether GitHub lists none or knows no such workflow.
    assert_red_for_every_workflow("not-run", |_| vec![]);
    let fx = fx("not-run-404");
    fx.green_but("beta.yml", vec![]);
    fx.put(vec![(runs_path("beta.yml"), Value::Null)]);
    let (code, _, err) = fx.gates();
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("beta.yml: no push run"), "{err}");
    let gone = fx.root.join("gh/gh-api.json");
    let mut map: Map<String, Value> =
        serde_json::from_str(&std::fs::read_to_string(&gone).unwrap()).unwrap();
    map.remove(&runs_path("beta.yml"));
    std::fs::write(&gone, Value::Object(map).to_string()).unwrap();
    let (code, _, err) = fx.gates();
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("beta.yml: no push run"), "{err}");
}

#[test]
fn a_green_run_of_another_commit_branch_or_event_is_not_this_commits() {
    // Catches: a run found by workflow alone: another sha, another branch (a fork's or a
    // topic branch's push), or a pull request run for the same sha.
    for (key, value) in [
        ("head_sha", json!(OTHER)),
        ("head_branch", json!("feature/x")),
        ("event", json!("pull_request")),
        ("event", json!("workflow_dispatch")),
        ("event", json!("schedule")),
    ] {
        assert_red_for_every_workflow(&format!("foreign-{key}-{value}"), |f| {
            vec![with(run(f, id_of(f)), key, value.clone())]
        });
    }
}

#[test]
fn a_foreign_run_beside_the_real_one_changes_nothing_but_a_foreign_green_cannot_stand_in() {
    // Catches: the first listed run taken instead of the matching one (both directions).
    let fx = fx("foreign-beside");
    let foreign = with(run("beta.yml", 99), "head_sha", json!(OTHER));
    let foreign = with(foreign, "run_number", json!(9));
    fx.green_but("beta.yml", vec![foreign.clone(), run("beta.yml", 22)]);
    let (code, out, err) = fx.gates();
    assert_eq!(code, Some(0), "{err}");
    assert!(out.contains("beta_run=22\n"), "{out}");
    fx.green_but("beta.yml", vec![foreign]);
    assert_eq!(fx.gates().0, Some(1));
}

#[test]
fn a_workflow_is_matched_by_its_path_never_by_its_name() {
    // Catches: a name-only match: a copied workflow (or one renamed to `ci`) whose run
    // is called what the real one is called, or a run with no path at all.
    let spoof = |f: &str, path: Value| {
        let mut r = run(f, id_of(f));
        r["path"] = path;
        r
    };
    assert_red_for_every_workflow("name-only", |f| {
        vec![spoof(f, json!(".github/workflows/copy.yml"))]
    });
    assert_red_for_every_workflow("other-dir", |f| {
        vec![spoof(f, json!(format!("elsewhere/{f}")))]
    });
    assert_red_for_every_workflow("path-suffix-name", |f| {
        vec![spoof(f, json!(format!(".github/workflows/x{f}")))]
    });
    assert_red_for_every_workflow("no-path", |f| {
        let mut r = run(f, id_of(f));
        r.as_object_mut().unwrap().remove("path");
        vec![r]
    });
    assert_red_for_every_workflow("other-ref-name", |f| {
        vec![spoof(
            f,
            json!(format!(".github/workflows/{f}.evil@refs/heads/main")),
        )]
    });
    // The file as another ref had it is not the file at this push.
    for (name, at) in [
        ("other-branch", "refs/heads/other".to_owned()),
        ("a-tag", "refs/tags/v1".to_owned()),
        ("a-sha", SHA.to_owned()),
        ("branch-prefix", "refs/heads/main2".to_owned()),
        ("empty-ref", String::new()),
    ] {
        assert_red_for_every_workflow(name, |f| {
            vec![spoof(f, json!(format!(".github/workflows/{f}@{at}")))]
        });
    }
}

#[test]
fn the_path_may_carry_the_ref_github_appends() {
    // Catches: a gate that is red forever because GitHub wrote `path@ref`.
    let first = fx("path-ref");
    first.green_but(
        "ci.yml",
        vec![with(
            run("ci.yml", 11),
            "path",
            json!(".github/workflows/ci.yml@refs/heads/main"),
        )],
    );
    let (code, _, err) = first.gates();
    assert_eq!(code, Some(0), "{err}");
    // Only the ref of the push being judged: `main` here, whatever GitHub wrote for `ci`.
    for file in ["beta.yml", "artifacts.yml"] {
        let fx = fx(&format!("path-ref-{file}"));
        let path = format!(".github/workflows/{file}@refs/heads/{BRANCH}");
        fx.green_but(
            file,
            vec![with(run(file, id_of(file)), "path", json!(path))],
        );
        let (code, _, err) = fx.gates();
        assert_eq!(code, Some(0), "{file}: {err}");
    }
}

#[test]
fn of_several_runs_the_latest_decides() {
    // Catches: the first, the earliest or the best run taken: a re-run or a second push of
    // the commit that failed after a success is red, and a success after a failure is green.
    let numbered = |f: &str, id: u64, number: u64, attempt: u64, conclusion: &str| {
        let r = with(run(f, id), "run_number", json!(number));
        let r = with(r, "run_attempt", json!(attempt));
        with(r, "conclusion", json!(conclusion))
    };
    for (file, runs, red) in [
        // A later run number, listed first or last.
        (
            "beta.yml",
            vec![
                numbered("beta.yml", 1, 5, 1, "success"),
                numbered("beta.yml", 2, 6, 1, "failure"),
            ],
            true,
        ),
        (
            "beta.yml",
            vec![
                numbered("beta.yml", 2, 6, 1, "failure"),
                numbered("beta.yml", 1, 5, 1, "success"),
            ],
            true,
        ),
        (
            "ci.yml",
            vec![
                numbered("ci.yml", 1, 5, 1, "failure"),
                numbered("ci.yml", 2, 6, 1, "success"),
            ],
            false,
        ),
        (
            "ci.yml",
            vec![
                numbered("ci.yml", 2, 6, 1, "success"),
                numbered("ci.yml", 1, 5, 1, "failure"),
            ],
            false,
        ),
        // The run number decides before the attempt: run 6 attempt 1 is newer than run 5
        // attempt 3, whatever their order in the list.
        (
            "beta.yml",
            vec![
                numbered("beta.yml", 1, 5, 3, "success"),
                numbered("beta.yml", 2, 6, 1, "failure"),
            ],
            true,
        ),
        (
            "beta.yml",
            vec![
                numbered("beta.yml", 2, 6, 1, "failure"),
                numbered("beta.yml", 1, 5, 3, "success"),
            ],
            true,
        ),
        (
            "ci.yml",
            vec![
                numbered("ci.yml", 1, 5, 3, "failure"),
                numbered("ci.yml", 2, 6, 1, "success"),
            ],
            false,
        ),
        // The same run, re-run: the higher attempt.
        (
            "artifacts.yml",
            vec![
                numbered("artifacts.yml", 3, 4, 1, "success"),
                numbered("artifacts.yml", 3, 4, 2, "cancelled"),
            ],
            true,
        ),
        (
            "artifacts.yml",
            vec![
                numbered("artifacts.yml", 3, 4, 2, "success"),
                numbered("artifacts.yml", 3, 4, 1, "failure"),
            ],
            false,
        ),
    ] {
        let fx = fx(&format!("latest-{file}-{red}-{}", runs[0]["id"]));
        fx.green_but(file, runs);
        let (code, out, err) = fx.gates();
        assert_eq!(code, Some(if red { 1 } else { 0 }), "{file}: {out}{err}");
    }
}

#[test]
fn the_latest_runs_id_is_the_one_printed() {
    // Catches: the id of an earlier run printed for the latest verdict.
    let fx = fx("latest-id");
    let later = with(run("ci.yml", 77), "run_number", json!(2));
    fx.green_but("ci.yml", vec![later, run("ci.yml", 11)]);
    let (code, out, err) = fx.gates();
    assert_eq!(code, Some(0), "{err}");
    assert!(out.starts_with("ci_run=77\n"), "{out}");
}

#[test]
fn a_re_run_that_is_going_is_waited_for_not_judged_by_the_run_before_it() {
    // Catches: an earlier green attempt standing in for a re-run that has not finished.
    let fx = fx("rerun-going");
    let going = with(
        with(run("beta.yml", 22), "status", json!("in_progress")),
        "conclusion",
        Value::Null,
    );
    let going = with(going, "run_attempt", json!(2));
    fx.green_but("beta.yml", vec![run("beta.yml", 22), going]);
    let (code, _, err) = fx.gates();
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("beta.yml: run 22 was still going"), "{err}");
}

#[test]
fn it_polls_until_every_run_completed_then_judges() {
    // Catches: a verdict on the first look (a run still going read as success or as red),
    // or a poll that stops early.
    let fx = fx("polling");
    fx.green_but("ci.yml", vec![run("ci.yml", 11)]);
    let going = |status: &str| {
        with(
            with(run("beta.yml", 22), "status", json!(status)),
            "conclusion",
            Value::Null,
        )
    };
    let look = |r: Value| json!({ "workflow_runs": [r] });
    fx.put(vec![(
        runs_path("beta.yml"),
        json!({ "$sequence": [
            look(going("queued")),
            look(going("in_progress")),
            look(run("beta.yml", 22)),
        ] }),
    )]);
    let started = Instant::now();
    let (code, out, err) = fx.gates_with(&[("--timeout-s", "60")]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(
        out,
        "ci_run=11\nbeta_run=22\nartifacts_run=33\nsuperseded=false\n"
    );
    assert!(started.elapsed().as_secs() >= 2, "it did not wait");
    let beta_looks = fx
        .gh_args()
        .lines()
        .filter(|l| l.contains("beta.yml"))
        .count();
    assert_eq!(beta_looks, 3);
}

#[test]
fn a_run_that_completes_red_while_polling_is_red() {
    // Catches: the first green look remembered across polls.
    let fx = fx("polling-red");
    fx.green_but("ci.yml", vec![run("ci.yml", 11)]);
    let going = with(
        with(run("beta.yml", 22), "status", json!("in_progress")),
        "conclusion",
        Value::Null,
    );
    let failed = with(run("beta.yml", 22), "conclusion", json!("failure"));
    fx.put(vec![(
        runs_path("beta.yml"),
        json!({ "$sequence": [
            { "workflow_runs": [going] },
            { "workflow_runs": [failed] },
        ] }),
    )]);
    let (code, _, err) = fx.gates_with(&[("--timeout-s", "60")]);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("concluded failure"), "{err}");
}

#[test]
fn a_run_still_going_at_the_timeout_is_red() {
    // Catches: a timeout read as pass; a timeout that never comes (the job would hang for
    // the runner's limit); an unfinished run named wrongly.
    for (timeout, poll) in [("0", "1"), ("1", "1"), ("1", "5")] {
        for (_, file) in GATES {
            let fx = fx(&format!("timeout-{timeout}-{poll}-{file}"));
            let going = with(
                with(run(file, id_of(file)), "status", json!("in_progress")),
                "conclusion",
                Value::Null,
            );
            fx.green_but(file, vec![going]);
            let started = Instant::now();
            let (code, out, err) = fx.gates_with(&[("--timeout-s", timeout), ("--poll-s", poll)]);
            assert_eq!(code, Some(1), "{out}{err}");
            assert!(
                err.contains(&format!(
                    "{file}: run {} was still going after {timeout} s",
                    id_of(file)
                )),
                "{err}"
            );
            assert_eq!(err.matches("still going").count(), 1, "{err}");
            assert!(out.is_empty());
            // A poll longer than the time left is cut to the time left.
            assert!(started.elapsed().as_secs() < 4, "slept past the timeout");
        }
    }
}

#[test]
fn a_red_run_ends_the_wait_without_waiting_for_the_rest() {
    // Catches: a wait for the whole timeout after a gate has already failed.
    let fx = fx("early-red");
    let going = with(
        with(run("beta.yml", 22), "status", json!("in_progress")),
        "conclusion",
        Value::Null,
    );
    fx.green_but("beta.yml", vec![going]);
    fx.lists(
        "ci.yml",
        vec![with(run("ci.yml", 11), "conclusion", json!("failure"))],
    );
    let started = Instant::now();
    let (code, _, err) = fx.gates_with(&[("--timeout-s", "3600"), ("--poll-s", "30")]);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("ci.yml: run 11"), "{err}");
    assert!(!err.contains("beta.yml"), "{err}");
    assert!(started.elapsed().as_secs() < 20);
}

#[test]
fn every_red_workflow_is_reported() {
    // Catches: only the first red gate named, so a fix of it reveals the next.
    let fx = fx("all-red");
    fx.green_but("ci.yml", vec![]);
    fx.lists(
        "beta.yml",
        vec![with(run("beta.yml", 22), "conclusion", json!("skipped"))],
    );
    let (_, _, err) = fx.gates();
    assert!(
        err.contains("ci.yml:") && err.contains("beta.yml:"),
        "{err}"
    );
    assert!(!err.contains("artifacts.yml:"), "{err}");
}

#[test]
fn a_run_without_an_id_is_red() {
    // Catches: a success with no run to name passing, so verdict-write gets no id.
    let fx = fx("no-id");
    let mut r = run("ci.yml", 11);
    r.as_object_mut().unwrap().remove("id");
    fx.green_but("ci.yml", vec![r]);
    let (code, _, err) = fx.gates();
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("ci.yml: its run"), "{err}");
}

#[test]
fn a_github_error_is_red_not_a_pass() {
    // Catches: an API failure read as "no problems found".
    let fx = fx("api-down");
    fx.green_but("ci.yml", vec![run("ci.yml", 11)]);
    let out = Command::new(XTASK)
        .args(["release", "gates", "--repo", REPO, "--sha", SHA])
        .args(["--branch", BRANCH, "--timeout-s", "0"])
        .arg("--gh")
        .arg(fx.root.join("gh/gh"))
        .env("XTASK_FAKE_DEFECT", "api-fails")
        .output()
        .expect("run xtask");
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("HTTP 502"));
}

#[test]
fn a_bad_argument_is_a_usage_error_before_any_call() {
    // Catches: a branch that breaks out of the query string, a short sha, a repo that is
    // not owner/name, a poll that spins.
    let fx = fx("usage");
    for (flag, value) in [
        ("--repo", "nope"),
        ("--sha", "abc123"),
        ("--branch", ""),
        ("--branch", "a b"),
        ("--branch", "main&event=pull_request"),
        ("--branch", "-x"),
        ("--branch", "/x"),
        ("--branch", "a/../b"),
        ("--poll-s", "0"),
    ] {
        let (code, out, err) = fx.gates_with(&[(flag, value)]);
        assert!(matches!(code, Some(1 | 2)), "{flag} {value:?}: {out}{err}");
        assert!(err.starts_with("xtask: "), "{flag} {value:?}: {err}");
        assert!(out.is_empty());
    }
    assert_eq!(fx.gh_args(), "", "a call was made");
}

#[test]
fn a_branch_with_a_slash_is_asked_for_as_it_is() {
    // Catches: a branch name mangled on the way to the query.
    let fx = fx("slash-branch");
    let path = |f: &str| {
        format!(
            "repos/{REPO}/actions/workflows/{f}/runs?head_sha={SHA}&event=push\
             &branch=release/x.y_z-1&per_page=100"
        )
    };
    for (_, f) in GATES {
        let r = with(run(f, id_of(f)), "head_branch", json!("release/x.y_z-1"));
        fx.put(vec![(path(f), json!({ "workflow_runs": [r] }))]);
    }
    let (code, _, err) = fx.gates_with(&[("--branch", "release/x.y_z-1")]);
    assert_eq!(code, Some(0), "{err}");
}

#[test]
fn a_full_page_of_runs_is_refused_not_judged_as_the_whole_list() {
    // Catches: the latest run on a page this verb does not read, with a green one on the
    // page it did: a full page is red, one run fewer is judged.
    let page = |n: usize| -> Vec<Value> {
        (0..n)
            .map(|i| with(run("ci.yml", 1000 + i as u64), "run_number", json!(i + 1)))
            .collect()
    };
    let fx1 = fx("page-full");
    fx1.green_but("ci.yml", page(100));
    let (code, out, err) = fx1.gates();
    assert_eq!(code, Some(1), "{out}{err}");
    assert!(
        err.contains("ci.yml:") && err.contains("full page"),
        "{err}"
    );
    let fx2 = fx("page-short");
    fx2.green_but("ci.yml", page(99));
    let (code, _, err) = fx2.gates();
    assert_eq!(code, Some(0), "{err}");
}

const NEWER: &str = "fedcba9876543210fedcba9876543210fedcba98";

#[test]
fn a_cancelled_run_on_a_commit_main_has_moved_past_ends_green_as_superseded() {
    // Each workflow in turn is the cancelled one, the other two green.
    for (_, file) in GATES {
        let fx = fx(&format!("superseded-{}", file.trim_end_matches(".yml")));
        fx.concluded(file, "cancelled");
        fx.main_is_at(NEWER, "ahead");
        let output = fx.root.join("out");
        let (code, out, err) = fx.gates_with(&[("--output", output.to_str().unwrap())]);
        assert_eq!(code, Some(0), "{file}: {err}");
        assert_eq!(
            out, "superseded=true\n",
            "no run ids for a superseded commit"
        );
        assert_eq!(
            std::fs::read_to_string(&output).unwrap(),
            "superseded=true\n"
        );
        assert!(
            err.contains(&format!(
                "superseded: {SHA} was replaced on {BRANCH} by {NEWER}; a later code release includes it"
            )),
            "{file}: {err}"
        );
    }
}

#[test]
fn several_cancelled_runs_on_a_replaced_commit_end_green_as_superseded() {
    let fx = fx("superseded-all");
    for (_, file) in GATES {
        fx.lists(
            file,
            vec![with(
                run(file, id_of(file)),
                "conclusion",
                json!("cancelled"),
            )],
        );
    }
    fx.main_is_at(NEWER, "ahead");
    let (code, out, err) = fx.gates();
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, "superseded=true\n");
}

#[test]
fn the_superseded_check_only_reads() {
    let fx = fx("superseded-reads");
    fx.concluded("beta.yml", "cancelled");
    fx.main_is_at(NEWER, "ahead");
    assert_eq!(fx.gates().0, Some(0));
    let args = fx.gh_args();
    assert!(
        args.contains(&format!("api repos/{REPO}/branches/{BRANCH}")),
        "{args}"
    );
    assert!(
        args.contains(&format!("api repos/{REPO}/compare/{SHA}...{NEWER}")),
        "{args}"
    );
    assert!(
        !args.contains(" -X ") && !args.contains("--input"),
        "{args}"
    );
}

#[test]
fn a_cancelled_run_on_the_head_of_main_stays_red() {
    for (_, file) in GATES {
        let fx = fx(&format!("cancelled-head-{}", file.trim_end_matches(".yml")));
        fx.concluded(file, "cancelled");
        fx.main_is_at(SHA, "identical");
        let (code, out, err) = fx.gates();
        assert_eq!(code, Some(1), "{file}: {out}{err}");
        assert!(err.contains(&format!("{file}: run")), "{err}");
        assert!(err.contains("concluded cancelled, not success"), "{err}");
        assert!(!err.contains("superseded"), "{err}");
        assert!(out.is_empty(), "{out}");
    }
}

#[test]
fn a_cancelled_run_stays_red_when_main_is_not_ahead_of_the_commit() {
    // The head is another commit but not a descendant: the commit is not on main's history,
    // so the newer commit's release would not include it.
    for status in ["diverged", "behind", "identical"] {
        let fx = fx(&format!("cancelled-{status}"));
        fx.concluded("ci.yml", "cancelled");
        fx.main_is_at(NEWER, status);
        let (code, out, err) = fx.gates();
        assert_eq!(code, Some(1), "{status}: {out}{err}");
        assert!(out.is_empty(), "{out}");
    }
}

#[test]
fn a_cancelled_run_stays_red_when_github_does_not_say_where_main_is() {
    // The fake answers 404 for the branch: nothing says a newer commit landed.
    let fx = fx("cancelled-no-head");
    fx.concluded("artifacts.yml", "cancelled");
    let (code, out, err) = fx.gates();
    assert_eq!(code, Some(1), "{out}{err}");
    assert!(err.contains("artifacts.yml:"), "{err}");
}

#[test]
fn a_cancelled_run_stays_red_when_the_head_is_not_a_full_commit() {
    let fx = fx("cancelled-odd-head");
    fx.concluded("ci.yml", "cancelled");
    fx.main_is_at("main", "ahead");
    assert_eq!(fx.gates().0, Some(1));
}

#[test]
fn a_failed_run_stays_red_even_when_the_commit_is_superseded() {
    for (_, file) in GATES {
        let fx = fx(&format!(
            "failed-superseded-{}",
            file.trim_end_matches(".yml")
        ));
        fx.concluded(file, "failure");
        fx.main_is_at(NEWER, "ahead");
        let (code, out, err) = fx.gates();
        assert_eq!(code, Some(1), "{file}: {out}{err}");
        assert!(err.contains(&format!("{file}: run")), "{err}");
        assert!(err.contains("concluded failure, not success"), "{err}");
        assert!(out.is_empty(), "{out}");
    }
}

#[test]
fn a_timed_out_run_stays_red_even_when_the_commit_is_superseded() {
    for (_, file) in GATES {
        let fx = fx(&format!(
            "timed-out-superseded-{}",
            file.trim_end_matches(".yml")
        ));
        fx.concluded(file, "timed_out");
        fx.main_is_at(NEWER, "ahead");
        let (code, out, err) = fx.gates();
        assert_eq!(code, Some(1), "{file}: {out}{err}");
        assert!(err.contains("concluded timed_out, not success"), "{err}");
        assert!(out.is_empty(), "{out}");
    }
}

#[test]
fn a_failure_beside_a_cancelled_run_stays_red_and_reports_both() {
    let fx = fx("failed-and-cancelled");
    fx.green_but(
        "ci.yml",
        vec![with(run("ci.yml", 11), "conclusion", json!("failure"))],
    );
    fx.lists(
        "beta.yml",
        vec![with(run("beta.yml", 22), "conclusion", json!("cancelled"))],
    );
    fx.main_is_at(NEWER, "ahead");
    let (code, _, err) = fx.gates();
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("ci.yml: run 11"), "{err}");
    assert!(err.contains("beta.yml: run 22"), "{err}");
}

#[test]
fn an_absent_run_stays_red_even_when_another_was_cancelled_and_the_commit_is_superseded() {
    let fx = fx("absent-and-cancelled");
    fx.concluded("ci.yml", "cancelled");
    fx.lists("beta.yml", vec![]);
    fx.main_is_at(NEWER, "ahead");
    let (code, out, err) = fx.gates();
    assert_eq!(code, Some(1), "{out}{err}");
    assert!(err.contains("beta.yml: no push run"), "{err}");
}

#[test]
fn all_green_runs_release_and_say_so_without_asking_where_main_is() {
    let fx = fx("green-not-superseded");
    fx.green_but("ci.yml", vec![run("ci.yml", 11)]);
    fx.main_is_at(NEWER, "ahead");
    let (code, out, err) = fx.gates();
    assert_eq!(code, Some(0), "{err}");
    assert!(out.ends_with("superseded=false\n"), "{out}");
    assert!(!fx.gh_args().contains("/branches/"), "{}", fx.gh_args());
}
