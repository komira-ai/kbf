//! `cargo xtask gamma` as the workflow runs it, with `xtask-fake` as `gh` and as `tailscale`.
//! The fake gh answers from its `gh-api.json`, GitHub's API as each test writes it. The fake
//! tailscale leaves what it was asked and given beside it and answers with the files a test
//! wrote: the installer's summary and its exit status.

use std::path::{Path, PathBuf};
use std::process::Command;

use kbf_testkit::dir::TestDir;
use serde_json::{Value, json};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/golden/gamma-held-steps.txt"
);
const REPO: &str = "komira-ai/kbf";
const SHA: &str = "c0ffee0123456789c0ffee0123456789c0ffee01";
const OTHER: &str = "badbad0123456789badbad0123456789badbad01";
const RUN: &str = "4242";
const HOST: &str = "canary-host";
const USER: &str = "canary-user";
const BUNDLE: &str = "the bundle bytes";

/// The exit code, stdout and stderr of a run.
type Ran = (Option<i32>, String, String);

fn s(p: &Path) -> &str {
    p.to_str().expect("utf-8 path")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn has(text: &str, needle: &str) {
    assert!(text.contains(needle), "{needle:?} is not in {text}");
}

fn lacks(text: &str, needle: &str) {
    assert!(!text.contains(needle), "{needle:?} is in {text}");
}

/// One test's world: the fake as `gh/gh` and as `ts/tailscale`, and the bundle.
fn world(name: &str) -> TestDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-gamma");
    let dir = TestDir::under(root, name);
    for (sub, program) in [("gh", "gh"), ("ts", "tailscale")] {
        std::fs::create_dir_all(dir.join(sub)).expect("directory");
        std::fs::hard_link(FAKE, dir.join(sub).join(program)).expect("link the fake");
    }
    std::fs::write(dir.join("bundle.tar"), BUNDLE).expect("the bundle");
    dir
}

/// `cargo xtask <args>`, with the fake planting `defect` (or none).
fn xtask_with(args: &[&str], defect: Option<&str>) -> Ran {
    let mut cmd = Command::new(XTASK);
    cmd.args(args);
    match defect {
        Some(d) => cmd.env("XTASK_FAKE_DEFECT", d),
        None => cmd.env_remove("XTASK_FAKE_DEFECT"),
    };
    let out = cmd.output().expect("run xtask");
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    (out.status.code(), text(&out.stdout), text(&out.stderr))
}

fn xtask(args: &[&str]) -> Ran {
    xtask_with(args, None)
}

/// Writes GitHub's answers, `(path, JSON)`, to the fake gh's `gh-api.json`.
fn answers(root: &Path, answers: &[(String, Value)]) {
    let map: serde_json::Map<String, Value> = answers.iter().cloned().collect();
    let text = Value::Object(map).to_string();
    std::fs::write(root.join("gh/gh-api.json"), text).expect("gh-api.json");
}

fn run_path() -> String {
    format!("repos/{REPO}/actions/runs/{RUN}")
}

fn jobs_path() -> String {
    format!("{}/jobs?per_page=100", run_path())
}

/// The release run the event named: a push to main that succeeded, at `SHA`.
fn run() -> Value {
    json!({
        "id": 4242,
        "path": ".github/workflows/release.yml",
        "name": "release",
        "event": "push",
        "head_branch": "main",
        "head_sha": SHA,
        "status": "completed",
        "conclusion": "success",
    })
}

/// `run()` with the fact `key` changed.
fn run_with(key: &str, value: Value) -> Value {
    let mut changed = run();
    changed[key] = value;
    changed
}

/// GitHub's list of the run's jobs, `(name, conclusion)`.
fn jobs(list: &[(&str, &str)]) -> Value {
    let list: Vec<Value> = list
        .iter()
        .map(|(name, conclusion)| json!({ "name": name, "conclusion": conclusion }))
        .collect();
    json!({ "jobs": list })
}

/// `gamma evidence` for `repo` and `sha`, its output going to `output`, with the fake gh
/// planting `defect` (or none).
fn evidence_of(root: &Path, repo: &str, sha: &str, output: &Path, defect: Option<&str>) -> Ran {
    xtask_with(
        &[
            "gamma",
            "evidence",
            "--repo",
            repo,
            "--sha",
            sha,
            "--run-id",
            RUN,
            "--output",
            s(output),
            "--gh",
            s(&root.join("gh/gh")),
        ],
        defect,
    )
}

/// `gamma evidence` for `SHA` into `<root>/github-output`.
fn evidence(root: &Path) -> Ran {
    evidence_of(root, REPO, SHA, &root.join("github-output"), None)
}

/// Proves the offer follows a release run that published: a successful push run of the
/// release file at the commit, with a publish job that succeeded, whichever way GitHub
/// writes the run's path. The answer reaches stdout and the output file.
#[test]
fn evidence_says_released_for_a_successful_push_run_that_published() {
    let paths = [
        ".github/workflows/release.yml",
        ".github/workflows/release.yml@refs/heads/main",
    ];
    for path in paths {
        let root = world("evidence-released");
        let listing = jobs(&[("verify", "success"), ("publish", "success")]);
        answers(
            &root,
            &[
                (run_path(), run_with("path", json!(path))),
                (jobs_path(), listing),
            ],
        );
        let (code, out, err) = evidence(&root);
        assert_eq!(code, Some(0), "{err}");
        assert_eq!(out, "released=true\n");
        assert_eq!(read(&root.join("github-output")), "released=true\n");
    }
}

/// Catches: a push that cut nothing (docs only: the publish job skipped), a publish job that
/// failed or is not there, or a run with no jobs listed, offering a release that was not made.
/// Each is a green run with `released=false`, not an error.
#[test]
fn evidence_says_not_released_when_the_publish_job_did_not_succeed() {
    let verify = ("verify", "success");
    let cases = [
        Some(jobs(&[verify, ("publish", "skipped")])),
        Some(jobs(&[verify, ("publish", "failure")])),
        Some(jobs(&[verify])),
        Some(json!({})),
        None,
    ];
    for listing in cases {
        let root = world("evidence-not-released");
        let mut known = vec![(run_path(), run())];
        known.extend(listing.map(|jobs| (jobs_path(), jobs)));
        answers(&root, &known);
        let (code, out, err) = evidence(&root);
        assert_eq!(code, Some(0), "{err}");
        assert_eq!(out, "released=false\n");
        assert_eq!(read(&root.join("github-output")), "released=false\n");
    }
}

/// Catches: a run that is not the one the event described passing as evidence: another file
/// or workflow name, a dispatch, another branch or commit, a run still going or one that
/// failed. Each is an error naming the fact, and nothing is written.
#[test]
fn evidence_refuses_a_run_that_contradicts_the_event() {
    let wrong = [
        ("path", json!(".github/workflows/other.yml")),
        ("name", json!("other")),
        ("event", json!("workflow_dispatch")),
        ("head_branch", json!("feature")),
        ("head_sha", json!(OTHER)),
        ("status", json!("in_progress")),
        ("conclusion", json!("failure")),
    ];
    for (key, value) in wrong {
        let root = world("evidence-contradicted");
        let listing = jobs(&[("publish", "success")]);
        answers(
            &root,
            &[(run_path(), run_with(key, value)), (jobs_path(), listing)],
        );
        let (code, _, err) = evidence(&root);
        assert_eq!(code, Some(1), "{key}: {err}");
        has(&err, &format!("the run's {key} is"));
        assert!(!root.join("github-output").exists(), "{key}");
    }
}

#[test]
fn evidence_refuses_a_run_that_is_not_there() {
    let root = world("evidence-missing");
    let (code, _, err) = evidence(&root);
    assert_eq!(code, Some(1));
    has(&err, "does not exist");
}

/// Catches: a malformed commit or repository reaching GitHub, and an output file that cannot
/// be written passing as a green step.
#[test]
fn evidence_checks_its_inputs_and_reports_an_unwritable_output() {
    let root = world("evidence-inputs");
    let output = root.join("github-output");
    assert_eq!(evidence_of(&root, REPO, "abc", &output, None).0, Some(2));
    assert_eq!(evidence_of(&root, "kbf", SHA, &output, None).0, Some(2));
    assert!(!root.join("gh/gh-args").exists());
    let listing = jobs(&[("publish", "success")]);
    answers(&root, &[(run_path(), run()), (jobs_path(), listing)]);
    let lost = root.join("no/dir/github-output");
    let (code, _, err) = evidence_of(&root, REPO, SHA, &lost, None);
    assert_eq!(code, Some(1));
    has(&err, "append to");
}

/// Catches: a GitHub call that failed (a 502, not a 404) read as "no run" or as "nothing was
/// published", which would end the offer green. The first call is the run, the second its
/// jobs; each failing alone is an error, and nothing is written.
#[test]
fn evidence_fails_when_github_does() {
    let root = world("evidence-github-fails");
    let listing = jobs(&[("publish", "success")]);
    answers(&root, &[(run_path(), run()), (jobs_path(), listing)]);
    let output = root.join("github-output");
    for defect in ["api-fails", "api-jobs-fail"] {
        let (code, _, err) = evidence_of(&root, REPO, SHA, &output, Some(defect));
        assert_eq!(code, Some(1), "{defect}: {err}");
        has(&err, "HTTP 502");
        assert!(!output.exists(), "{defect}");
    }
}

/// GitHub's list of the release tags `list`.
fn tags(list: &[&str]) -> (String, Value) {
    let refs: Vec<Value> = list
        .iter()
        .map(|t| json!({ "ref": format!("refs/tags/{t}") }))
        .collect();
    let path = format!("repos/{REPO}/git/matching-refs/tags/v?per_page=100&page=1");
    (path, Value::Array(refs))
}

/// GitHub's compare of the tag `tag` against `SHA`.
fn compare(tag: &str, status: &str) -> (String, Value) {
    let path = format!("repos/{REPO}/compare/{tag}...{SHA}");
    (path, json!({ "status": status }))
}

/// What the fake installer prints: its summary, with `canary` words where a public log must
/// not show a node, a free text or a refusal.
fn installer_summary(result: &str, exit: i64) -> String {
    json!({
        "schema": 2,
        "commit": SHA,
        "result": result,
        "exit": exit,
        "reason": "",
        "detail": "canary-detail",
        "steps": [{ "name": "bundle", "ok": true, "ms": 3 }],
        "nodes": [{
            "id": "canary-node",
            "platform": "linux-x86_64",
            "state": "confirmed",
            "running_commit": SHA,
            "secs": 4,
            "refusal": "canary-refusal",
        }],
        "post": {
            "members_active": true,
            "nodes_on_commit": true,
            "readyz": true,
            "cas_smoke": "pass",
        },
    })
    .to_string()
}

/// What `gamma deploy` is run with: the defaults of a world, a field changed per test.
struct Call<'a> {
    repo: &'a str,
    sha: &'a str,
    host: &'a str,
    user: &'a str,
    bundle: PathBuf,
    raw: PathBuf,
    summary: PathBuf,
    tailscale: PathBuf,
}

/// The call of a world, with `edit` applied.
fn call(root: &Path, edit: impl FnOnce(&mut Call<'static>)) -> Call<'static> {
    let mut c = Call {
        repo: REPO,
        sha: SHA,
        host: HOST,
        user: USER,
        bundle: root.join("bundle.tar"),
        raw: root.join("raw.json"),
        summary: root.join("summary.json"),
        tailscale: root.join("ts/tailscale"),
    };
    edit(&mut c);
    c
}

fn deploy(root: &Path, c: &Call<'_>) -> Ran {
    xtask(&[
        "gamma",
        "deploy",
        "--repo",
        c.repo,
        "--sha",
        c.sha,
        "--host",
        c.host,
        "--user",
        c.user,
        "--bundle",
        s(&c.bundle),
        "--raw",
        s(&c.raw),
        "--summary",
        s(&c.summary),
        "--tailscale",
        s(&c.tailscale),
        "--gh",
        s(&root.join("gh/gh")),
    ])
}

/// A world where the commit is the highest release and the installer succeeds.
fn released(name: &str) -> TestDir {
    let root = world(name);
    answers(&root, &[tags(&["v0.1.2"]), compare("v0.1.2", "identical")]);
    let summary = installer_summary("ok", 0);
    std::fs::write(root.join("ts/tailscale-stdout"), summary).expect("stdout");
    root
}

/// Proves a deploy of the highest release masks the host and user first, runs the one
/// command through `tailscale ssh` with the bundle on its standard input, keeps the
/// installer's own summary only in the raw file and writes the redacted one. Catches a deploy
/// that sends another command, loses the bundle, or lets a node id or a free text into the
/// summary that is uploaded.
#[test]
fn deploy_runs_the_one_command_with_the_bundle_in_and_writes_only_the_redacted_summary() {
    let root = released("deploy");
    let (code, out, err) = deploy(&root, &call(&root, |_| {}));
    assert_eq!(code, Some(0), "{err}");
    let masks = format!("::add-mask::{HOST}\n::add-mask::{USER}\n");
    assert!(out.starts_with(&masks), "{out}");
    let command = "/usr/bin/kbf-installer deploy --apply --json --commit";
    let sent = format!("ssh {USER}@{HOST} sudo -n {command} {SHA}");
    let want = format!("{sent} --offer - --bundle -\n");
    assert_eq!(read(&root.join("ts/tailscale-args")), want);
    assert_eq!(read(&root.join("ts/tailscale-stdin")), BUNDLE);
    has(&read(&root.join("raw.json")), "canary-node");
    let kept = read(&root.join("summary.json"));
    lacks(&kept, "canary");
    let kept: Value = serde_json::from_str(&kept).expect("redacted JSON");
    assert_eq!(kept["result"], "ok");
    assert_eq!(kept["commit"], SHA);
    assert_eq!(kept["nodes"][0]["state"], "confirmed");
    for log in [&out, &err] {
        lacks(log, "canary-node");
    }
}

/// Catches: an installer that failed ending the step green, a failure that names the host,
/// and a failed deploy that leaves no summary for the table.
#[test]
fn deploy_fails_with_the_installers_status_and_still_writes_the_summary() {
    let root = released("deploy-failed");
    let summary = installer_summary("failed", 3);
    std::fs::write(root.join("ts/tailscale-stdout"), summary).expect("stdout");
    std::fs::write(root.join("ts/tailscale-exit"), "3\n").expect("exit");
    let (code, _, err) = deploy(&root, &call(&root, |_| {}));
    assert_eq!(code, Some(1));
    has(&err, "exited 3");
    lacks(&err, HOST);
    lacks(&err, USER);
    let kept = read(&root.join("summary.json"));
    lacks(&kept, "canary");
    let kept: Value = serde_json::from_str(&kept).expect("redacted JSON");
    assert_eq!(kept["result"], "failed");
}

/// Proves Rule S for the farm write: the deploy asks nothing of the host unless the commit is
/// the commit of the highest release tag. Catches a deploy of a commit that is only ahead of
/// the highest tag (nothing was released at it), of one with no release at all, and of an old
/// run's commit when a higher tag exists: in each the fake tailscale was never run.
#[test]
fn deploy_refuses_before_ssh_unless_the_commit_is_the_highest_release() {
    let ahead = vec![tags(&["v0.1.2"]), compare("v0.1.2", "ahead")];
    let untagged = vec![tags(&[])];
    let behind = vec![tags(&["v0.1.2", "v0.1.10"]), compare("v0.1.10", "behind")];
    let diverged = vec![tags(&["v0.1.2"]), compare("v0.1.2", "diverged")];
    for (name, known) in [
        ("ahead", ahead),
        ("untagged", untagged),
        ("behind", behind),
        ("diverged", diverged),
    ] {
        let root = world(&format!("deploy-{name}"));
        answers(&root, &known);
        let (code, _, err) = deploy(&root, &call(&root, |_| {}));
        assert_eq!(code, Some(1), "{name}: {err}");
        assert!(!root.join("ts/tailscale-args").exists(), "{name}");
        assert!(!root.join("raw.json").exists(), "{name}");
        assert!(!root.join("summary.json").exists(), "{name}");
    }
}

/// Catches: a root account, an address for a host, a malformed commit or repository reaching
/// GitHub or the host, and a name printed as a mask before it was checked.
#[test]
fn deploy_checks_every_input_before_it_prints_a_name_or_runs_anything() {
    let root = world("deploy-usage");
    let calls = [
        call(&root, |c| c.user = "root"),
        call(&root, |c| c.user = "Not A Name"),
        call(&root, |c| c.host = "198.51.100.7"),
        call(&root, |c| c.sha = "abc"),
        call(&root, |c| c.repo = "kbf"),
    ];
    for c in calls {
        let (code, out, err) = deploy(&root, &c);
        assert_eq!(code, Some(2), "{err}");
        lacks(&out, "::add-mask::");
        assert!(!root.join("gh/gh-args").exists());
        assert!(!root.join("ts/tailscale-args").exists());
    }
}

/// Catches: a failure after the release check ending the step without a message: a bundle
/// that is not there, a `tailscale` that does not start, a raw or summary file that cannot
/// be written.
#[test]
fn deploy_reports_a_missing_bundle_program_or_output() {
    let root = released("deploy-broken");
    let lost = root.join("no/dir/file");
    let shown = lost.display();
    let needles = [
        format!("open {shown}"),
        "tailscale: ".to_owned(),
        format!("create {shown}"),
        format!("write {shown}"),
    ];
    let calls = [
        call(&root, |c| c.bundle = lost.clone()),
        call(&root, |c| c.tailscale = lost.clone()),
        call(&root, |c| c.raw = lost.clone()),
        call(&root, |c| c.summary = lost.clone()),
    ];
    for (needle, c) in needles.iter().zip(calls) {
        let (code, _, err) = deploy(&root, &c);
        assert_eq!(code, Some(1), "{needle}: {err}");
        has(&err, needle);
    }
}

#[test]
fn sudoers_line_prints_the_rule_and_refuses_root() {
    let (code, out, err) = xtask(&["gamma", "sudoers-line", "--user", "kbf-deploy"]);
    assert_eq!(code, Some(0), "{err}");
    let line = xtask::gamma::sudoers_line("kbf-deploy").unwrap();
    assert_eq!(out, format!("{line}\n"));
    let (code, _, err) = xtask(&["gamma", "sudoers-line", "--user", "root"]);
    assert_eq!(code, Some(2));
    has(&err, "--user");
}

/// Proves the held steps, for the repository and names of the golden, are the golden's text
/// (the file is what a person is given to run); a bad name is a usage error, and the verb has
/// no way to apply anything.
#[test]
fn held_steps_print_the_golden_and_apply_nothing() {
    let named = |host: &str, extra: &[&str]| {
        let mut args = vec!["gamma", "held-steps", "--repo", REPO, "--host", host];
        args.extend(["--user", "kbf-deploy"]);
        args.extend(extra);
        xtask(&args)
    };
    let (code, out, err) = named("farm-front", &[]);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, read(Path::new(GOLDEN)));
    let (code, _, err) = named("198.51.100.7", &[]);
    assert_eq!(code, Some(2));
    has(&err, "--host");
    let (code, _, err) = named("farm-front", &["--tag", "Tag:x"]);
    assert_eq!(code, Some(2));
    has(&err, "--tag");
    let (code, _, err) = named("farm-front", &["--apply"]);
    assert_eq!(code, Some(2));
    has(&err, "--apply");
}

#[test]
fn summary_appends_the_table_of_the_redacted_summary() {
    let root = world("summary");
    let (file, out) = (root.join("summary.json"), root.join("step-summary.md"));
    let redacted = xtask::gamma::redact(&installer_summary("ok", 0));
    std::fs::write(&file, redacted).expect("summary");
    let (code, _, err) = xtask(&["gamma", "summary", "--file", s(&file), "--output", s(&out)]);
    assert_eq!(code, Some(0), "{err}");
    has(&read(&out), "| linux-x86_64 | confirmed | 4 |");
}
