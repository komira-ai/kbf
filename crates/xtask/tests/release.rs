//! `cargo xtask release` run as the release workflow runs it, with `xtask-fake` as `gh`:
//! its `gh-api.json` is GitHub's API as each test writes it, and its `releases/`
//! directory the releases `gh release` made.

use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::Digest;

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const REPO: &str = "komira-ai/komira-build-farm";
const COMMIT: &str = "c0ffee0123456789c0ffee0123456789c0ffee01";
const OTHER: &str = "badbad0123456789badbad0123456789badbad01";
const RUN: u64 = 4242;
const TAG: &str = "v0.1.0";
const ARTIFACTS: [&str; 4] = [
    "assets-darwin-arm64",
    "assets-linux-x86_64",
    "assets-linux-arm64",
    "sha256sums",
];

/// An empty directory of its own for one test, holding the fake as `gh/gh` and an
/// empty `tmp/`.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("xtask-release")
        .join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("clear scratch");
    }
    std::fs::create_dir_all(dir.join("gh")).expect("scratch");
    std::fs::create_dir_all(dir.join("tmp")).expect("tmp");
    std::fs::hard_link(FAKE, dir.join("gh/gh")).expect("link the fake as gh");
    dir
}

fn s(p: &Path) -> &str {
    p.to_str().expect("utf-8 path")
}

/// The exit code, stdout and stderr of `cargo xtask <args>` with `<root>/gh/gh` as
/// `gh`, planting `defect` in it (or none).
fn xtask(root: &Path, args: &[&str], defect: Option<&str>) -> (Option<i32>, String, String) {
    let gh = root.join("gh/gh");
    let mut cmd = Command::new(XTASK);
    cmd.args(args)
        .arg("--gh")
        .arg(&gh)
        .env("TMPDIR", root.join("tmp"));
    match defect {
        Some(d) => cmd.env("XTASK_FAKE_DEFECT", d),
        None => cmd.env_remove("XTASK_FAKE_DEFECT"),
    };
    let out = cmd.output().expect("run xtask");
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    (out.status.code(), text(&out.stdout), text(&out.stderr))
}

/// `cargo xtask release check`, which takes no `--gh`.
fn check(dir: &Path, commit: &str) -> (Option<i32>, String, String) {
    let out = Command::new(XTASK)
        .args(["release", "check", s(dir), "--commit", commit])
        .output()
        .expect("run xtask");
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    (out.status.code(), text(&out.stdout), text(&out.stderr))
}

/// Writes GitHub's answers, `(path, JSON)`, to the fake's `gh-api.json`.
fn answers(root: &Path, answers: &[(String, serde_json::Value)]) {
    let map: serde_json::Map<String, serde_json::Value> = answers.iter().cloned().collect();
    std::fs::write(
        root.join("gh/gh-api.json"),
        serde_json::Value::Object(map).to_string(),
    )
    .expect("gh-api.json");
}

/// What the fake gh was asked, one line per command.
fn gh_args(root: &Path) -> String {
    std::fs::read_to_string(root.join("gh/gh-args")).unwrap_or_default()
}

fn compare(commit: &str, status: &str) -> (String, serde_json::Value) {
    (
        format!("repos/{REPO}/compare/main...{commit}"),
        serde_json::json!({ "status": status }),
    )
}

fn runs_path(commit: &str) -> String {
    format!(
        "repos/{REPO}/actions/workflows/artifacts.yml/runs?head_sha={commit}&event=push\
         &branch=main&status=success&per_page=100"
    )
}

/// A run of the artifacts workflow: `RUN` for `COMMIT`, a push on main that succeeded,
/// with `changed` set on top.
fn run(changed: serde_json::Value) -> serde_json::Value {
    let mut r = serde_json::json!({
        "id": RUN, "head_sha": COMMIT, "head_branch": "main", "event": "push",
        "conclusion": "success",
    });
    for (k, v) in changed.as_object().expect("an object") {
        r[k] = v.clone();
    }
    r
}

fn runs(list: Vec<serde_json::Value>) -> (String, serde_json::Value) {
    (
        runs_path(COMMIT),
        serde_json::json!({ "workflow_runs": list }),
    )
}

fn artifacts(list: &[(&str, bool)]) -> (String, serde_json::Value) {
    let list: Vec<serde_json::Value> = list
        .iter()
        .map(|(n, expired)| serde_json::json!({ "name": n, "expired": expired }))
        .collect();
    (
        format!("repos/{REPO}/actions/runs/{RUN}/artifacts?per_page=100"),
        serde_json::json!({ "artifacts": list }),
    )
}

fn all_artifacts() -> (String, serde_json::Value) {
    artifacts(&ARTIFACTS.map(|a| (a, false)))
}

fn locate(root: &Path, commit: &str, defect: Option<&str>) -> (Option<i32>, String, String) {
    let output = root.join("github-output");
    xtask(
        root,
        &[
            "release",
            "locate",
            "--repo",
            REPO,
            "--commit",
            commit,
            "--output",
            s(&output),
        ],
        defect,
    )
}

#[test]
fn locate_finds_the_successful_main_push_run_of_the_commit() {
    // Control: catches a locate that refuses a commit on main (`main` ahead of it, or
    // at it), takes a decoy run (another commit, branch or event, or a failed run)
    // listed before the right one, or does not hand the run on to the next steps.
    for status in ["behind", "identical"] {
        let root = scratch(&format!("locate-{status}"));
        let decoys = [
            serde_json::json!({ "id": 1, "head_sha": OTHER }),
            serde_json::json!({ "id": 2, "head_branch": "feature" }),
            serde_json::json!({ "id": 3, "event": "workflow_dispatch" }),
            serde_json::json!({ "id": 4, "conclusion": "failure" }),
        ];
        let mut list: Vec<serde_json::Value> = decoys.into_iter().map(run).collect();
        list.push(run(serde_json::json!({})));
        let mut kept = ARTIFACTS.map(|a| (a, false)).to_vec();
        kept.push(("sha256sums", true));
        answers(
            &root,
            &[compare(COMMIT, status), runs(list), artifacts(&kept)],
        );
        let (code, out, err) = locate(&root, COMMIT, None);
        assert_eq!(code, Some(0), "{err}");
        assert_eq!(
            out,
            format!("{COMMIT} is on main; run {RUN} of artifacts.yml built it\nrun-id={RUN}\n")
        );
        let output = std::fs::read_to_string(root.join("github-output")).expect("output");
        assert_eq!(output, format!("run-id={RUN}\n"));
    }
}

#[test]
fn locate_without_output_only_prints_the_run() {
    // Catches: a locate that needs `--output` to print the run it found.
    let root = scratch("locate-no-output");
    answers(
        &root,
        &[
            compare(COMMIT, "behind"),
            runs(vec![run(serde_json::json!({}))]),
            all_artifacts(),
        ],
    );
    let (code, out, err) = xtask(
        &root,
        &["release", "locate", "--repo", REPO, "--commit", COMMIT],
        None,
    );
    assert_eq!(code, Some(0), "{err}");
    assert!(out.ends_with(&format!("run-id={RUN}\n")), "{out}");
}

#[test]
fn locate_refuses_a_commit_that_is_not_on_main() {
    // Catches: a tag on a branch commit (`main` behind it or diverged from it) or on a
    // commit GitHub does not know released, and a check made after looking for runs.
    for (name, status) in [
        ("ahead", Some("ahead")),
        ("diverged", Some("diverged")),
        ("unknown", None),
    ] {
        let root = scratch(&format!("locate-off-main-{name}"));
        if let Some(status) = status {
            answers(
                &root,
                &[
                    compare(COMMIT, status),
                    runs(vec![run(serde_json::json!({}))]),
                ],
            );
        }
        let (code, out, err) = locate(&root, COMMIT, None);
        assert_eq!(code, Some(1), "{name}: {out}");
        assert_eq!(
            err,
            format!(
                "xtask: {COMMIT} is not on main: a release is cut only from a commit on main\n"
            ),
            "{name}"
        );
        assert!(!gh_args(&root).contains("runs"), "{name}: looked for runs");
        assert!(
            !root.join("github-output").exists(),
            "{name}: wrote an output"
        );
    }
}

#[test]
fn locate_refuses_a_commit_with_no_successful_main_push_run() {
    // Catches: a decoy run (another commit, another branch, another event, a failed
    // run) taken as the one that built the commit, and no runs at all taken for one.
    let decoys = [
        serde_json::json!({ "head_sha": OTHER }),
        serde_json::json!({ "head_branch": "feature" }),
        serde_json::json!({ "event": "pull_request" }),
        serde_json::json!({ "conclusion": "cancelled" }),
        serde_json::json!({ "id": "not a number" }),
    ];
    let mut cases: Vec<(String, Option<Vec<serde_json::Value>>)> =
        vec![("none".to_owned(), Some(vec![])), ("404".to_owned(), None)];
    for (i, d) in decoys.into_iter().enumerate() {
        cases.push((format!("decoy-{i}"), Some(vec![run(d)])));
    }
    for (name, list) in cases {
        let root = scratch(&format!("locate-no-run-{name}"));
        let mut given = vec![compare(COMMIT, "behind")];
        given.extend(list.map(runs));
        answers(&root, &given);
        let (code, _, err) = locate(&root, COMMIT, None);
        assert_eq!(code, Some(1), "{name}");
        assert_eq!(
            err,
            format!("xtask: no successful push run of artifacts.yml on main for {COMMIT}\n"),
            "{name}"
        );
    }
}

#[test]
fn locate_refuses_a_run_whose_artifacts_are_gone() {
    // Catches: a run whose artifacts expired (after 90 days) or are missing taken, so
    // the download fails later with a less clear message, or a partial set released.
    let root = scratch("locate-expired");
    answers(
        &root,
        &[
            compare(COMMIT, "behind"),
            runs(vec![run(serde_json::json!({}))]),
            artifacts(&[
                ("assets-darwin-arm64", true),
                ("assets-linux-x86_64", false),
                ("assets-linux-arm64", false),
            ]),
        ],
    );
    let (code, _, err) = locate(&root, COMMIT, None);
    assert_eq!(code, Some(1));
    assert_eq!(
        err,
        format!(
            "xtask: run {RUN} of artifacts.yml holds no unexpired artifact assets-darwin-arm64\n\
             run {RUN} of artifacts.yml holds no unexpired artifact sha256sums\n"
        )
    );
    // No artifacts listing at all: every one is missing.
    let root = scratch("locate-no-artifacts");
    answers(
        &root,
        &[
            compare(COMMIT, "behind"),
            runs(vec![run(serde_json::json!({}))]),
        ],
    );
    let (code, _, err) = locate(&root, COMMIT, None);
    assert_eq!(code, Some(1));
    assert_eq!(err.lines().count(), 4, "{err}");
}

#[test]
fn locate_refuses_what_is_not_a_full_commit() {
    // Catches: a short SHA (it can name another commit later) or a branch name
    // accepted, which GitHub's compare would resolve as a ref.
    let root = scratch("locate-short");
    for bad in [&COMMIT[..12], "main", &"g".repeat(40)] {
        let (code, _, err) = locate(&root, bad, None);
        assert_eq!(code, Some(2), "{bad}");
        assert!(err.contains("is not a full commit SHA"), "{bad}: {err}");
    }
    assert_eq!(gh_args(&root), "", "asked GitHub about a malformed commit");
}

#[test]
fn an_api_failure_fails_the_run_rather_than_reading_as_an_answer() {
    // Catches: a 5xx or a body that is not JSON read as "not on main" or "no run",
    // which would hide an outage behind a refusal that blames the commit.
    for (defect, want) in [
        ("api-fails", "gh: Server Error (HTTP 502)"),
        ("api-garbage", "not JSON"),
    ] {
        let root = scratch(&format!("api-{defect}"));
        let (code, _, err) = locate(&root, COMMIT, Some(defect));
        assert_eq!(code, Some(1), "{defect}");
        assert!(
            err.starts_with(&format!(
                "xtask: gh api repos/{REPO}/compare/main...{COMMIT}: "
            )),
            "{err}"
        );
        assert!(err.contains(want), "{defect}: {err}");
    }
}

#[test]
fn a_gh_that_cannot_be_started_fails_the_run() {
    // Catches: a gh that cannot be started read as a 404 (so as "not on main").
    let root = scratch("no-gh");
    let absent = root.join("absent");
    let out = Command::new(XTASK)
        .args([
            "release",
            "locate",
            "--repo",
            REPO,
            "--commit",
            COMMIT,
            "--gh",
            s(&absent),
        ])
        .output()
        .expect("run xtask");
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.starts_with(&format!("xtask: {}: ", absent.display())),
        "{err}"
    );
}

/// `release main-control` with `on` and `off` answered by the statuses given.
fn main_control(name: &str, on: &str, off: &str) -> (Option<i32>, String, String) {
    let root = scratch(name);
    answers(&root, &[compare(COMMIT, on), compare(OTHER, off)]);
    xtask(
        &root,
        &[
            "release",
            "main-control",
            "--repo",
            REPO,
            "--on-main",
            COMMIT,
            "--off-main",
            OTHER,
        ],
        None,
    )
}

#[test]
fn main_control_passes_when_the_check_tells_on_from_off() {
    // Control: catches a main-control that fails a correct check.
    let (code, out, err) = main_control("control-good", "behind", "ahead");
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(
        out,
        format!("on-main check: {COMMIT} is accepted and {OTHER} is refused\n")
    );
}

#[test]
fn main_control_fails_a_check_that_refuses_main_or_accepts_a_branch() {
    // Catches: the check inverted, or reading `ahead` as on main (each a planted
    // answer the real check must judge as the control expects).
    let (code, _, err) = main_control("control-refuses-main", "ahead", "ahead");
    assert_eq!(code, Some(1));
    assert_eq!(
        err,
        format!("xtask: control failed: {COMMIT}, on main, was refused\n")
    );
    let (code, _, err) = main_control("control-accepts-branch", "identical", "behind");
    assert_eq!(code, Some(1));
    assert_eq!(
        err,
        format!("xtask: control failed: {OTHER}, not on main, was accepted\n")
    );
    let root = scratch("control-short");
    let (code, _, err) = xtask(
        &root,
        &[
            "release",
            "main-control",
            "--repo",
            REPO,
            "--on-main",
            COMMIT,
            "--off-main",
            "abc",
        ],
        None,
    );
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("--off-main \"abc\""), "{err}");
}

/// The assets of `commit` as the artifacts run leaves them, in `<root>/dist`: a tarball
/// and an SBOM per platform, and `SHA256SUMS` over them.
fn dist(root: &Path, commit: &str) -> PathBuf {
    let dist = root.join("dist");
    std::fs::create_dir_all(&dist).expect("dist");
    let mut sums = String::new();
    for platform in ["darwin-arm64", "linux-arm64", "linux-x86_64"] {
        for ext in ["cdx.json", "tar.gz"] {
            let name = format!("kbf-daemon-{commit}-{platform}.{ext}");
            let bytes = format!("{name} bytes");
            std::fs::write(dist.join(&name), &bytes).expect("asset");
            sums.push_str(&format!(
                "{}  {name}\n",
                hex::encode(sha2::Sha256::digest(&bytes))
            ));
        }
    }
    std::fs::write(dist.join("SHA256SUMS"), sums).expect("SHA256SUMS");
    dist
}

#[test]
fn check_passes_the_assets_of_the_commit() {
    // Control: catches a check that refuses a correct directory.
    let root = scratch("check-good");
    let dist = dist(&root, COMMIT);
    let (code, out, err) = check(&dist, COMMIT);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(
        out,
        format!(
            "sums: 6 files match; the flipped copy does not\n\
             names: every listed file is an asset of {COMMIT}\n"
        )
    );
}

#[test]
fn check_refuses_assets_of_another_commit_or_of_no_binary() {
    // Catches: the artifacts of another run (another commit) released under this tag,
    // and a listed file that is not `<binary>-<commit>-<platform>` with a known
    // platform and extension.
    let root = scratch("check-other");
    let other_dist = dist(&root, OTHER);
    let (code, _, err) = check(&other_dist, COMMIT);
    assert_eq!(code, Some(1));
    assert_eq!(err.lines().count(), 6, "{err}");
    assert!(
        err.contains(&format!(
            "kbf-daemon-{OTHER}-linux-arm64.tar.gz: not an asset of {COMMIT}"
        )),
        "{err}"
    );
    for bad in [
        format!("-{COMMIT}-linux-arm64.tar.gz"),
        format!("kbf-daemon-{COMMIT}-windows-x86_64.tar.gz"),
        format!("kbf-daemon-{COMMIT}-linux-arm64.zip"),
        "notes.txt".to_owned(),
    ] {
        let root = scratch("check-bad-name");
        let commit_dist = dist(&root, COMMIT);
        let sums = std::fs::read_to_string(commit_dist.join("SHA256SUMS")).expect("sums");
        std::fs::write(commit_dist.join(&bad), "x").expect("file");
        let line = format!("{}  {bad}\n", hex::encode(sha2::Sha256::digest(b"x")));
        std::fs::write(commit_dist.join("SHA256SUMS"), format!("{line}{sums}")).expect("sums");
        let (code, _, err) = check(&commit_dist, COMMIT);
        assert_eq!(code, Some(1), "{bad}");
        assert_eq!(err, format!("xtask: {bad}: not an asset of {COMMIT}\n"));
    }
}

#[test]
fn check_refuses_a_changed_file() {
    // Catches: a check that reads names only (the `sha256sum --check` half dropped).
    let root = scratch("check-changed");
    let dist = dist(&root, COMMIT);
    std::fs::write(
        dist.join(format!("kbf-daemon-{COMMIT}-linux-arm64.tar.gz")),
        "other",
    )
    .expect("change");
    let (code, _, err) = check(&dist, COMMIT);
    assert_eq!(code, Some(1));
    assert!(err.contains("does not match its line"), "{err}");
    let (code, _, err) = check(&dist, "abc");
    assert_eq!(code, Some(2), "{err}");
}

fn publish(root: &Path, defect: Option<&str>) -> (Option<i32>, String, String) {
    let dist = root.join("dist");
    let run = RUN.to_string();
    xtask(
        root,
        &[
            "release",
            "publish",
            s(&dist),
            "--repo",
            REPO,
            "--tag",
            TAG,
            "--commit",
            COMMIT,
            "--run-id",
            &run,
        ],
        defect,
    )
}

/// The release `TAG` as the fake holds it: file name and bytes, sorted.
fn released(root: &Path) -> Vec<(String, Vec<u8>)> {
    let dir = root.join("gh/releases").join(TAG);
    let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .map(|e| {
                    let e = e.expect("entry");
                    let bytes = std::fs::read(e.path()).expect("asset");
                    (e.file_name().to_string_lossy().into_owned(), bytes)
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

/// The files of `<root>/dist`: name and bytes, sorted.
fn verified(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(root.join("dist"))
        .expect("dist")
        .map(|e| {
            let e = e.expect("entry");
            (
                e.file_name().to_string_lossy().into_owned(),
                std::fs::read(e.path()).expect("file"),
            )
        })
        .collect();
    files.sort();
    files
}

#[test]
fn publish_creates_the_release_with_every_verified_file() {
    // Catches: a release created without the tag check (`--verify-tag`), without some
    // file (SHA256SUMS included), or with notes that do not say how to verify.
    let root = scratch("publish-new");
    dist(&root, COMMIT);
    let (code, out, err) = publish(&root, None);
    assert_eq!(code, Some(0), "{err}");
    assert!(
        out.ends_with(&format!("created release {TAG} with 7 files\n")),
        "{out}"
    );
    assert_eq!(released(&root), verified(&root));
    let args = gh_args(&root);
    let create = args
        .lines()
        .find(|l| l.starts_with("release create"))
        .expect("a create");
    assert!(
        create.starts_with(&format!(
            "release create {TAG} --repo {REPO} --verify-tag --title {TAG} --notes "
        )),
        "{create}"
    );
    assert!(
        create.contains(&format!("/actions/runs/{RUN})")),
        "{create}"
    );
    // The notes span lines of the log; the verify command is one of them.
    assert!(
        args.contains(&format!(
            "--signer-workflow {REPO}/.github/workflows/artifacts.yml --source-ref refs/heads/main --deny-self-hosted-runners"
        )),
        "{args}"
    );
}

#[test]
fn a_rerun_changes_nothing() {
    // Catches: a re-run that creates a second release, uploads an asset again, or
    // replaces one (`--clobber`).
    let root = scratch("publish-rerun");
    dist(&root, COMMIT);
    let (code, _, err) = publish(&root, None);
    assert_eq!(code, Some(0), "{err}");
    let before = released(&root);
    let (code, out, err) = publish(&root, None);
    assert_eq!(code, Some(0), "{err}");
    assert!(
        out.ends_with(&format!(
            "release {TAG}: 7 files already held byte for byte, 0 uploaded\n"
        )),
        "{out}"
    );
    assert_eq!(released(&root), before);
    let args = gh_args(&root);
    assert_eq!(args.matches("release create").count(), 1, "{args}");
    assert!(!args.contains("release upload"), "{args}");
    assert!(!args.contains("--clobber"), "{args}");
}

#[test]
fn a_rerun_uploads_only_what_the_release_lacks() {
    // Catches: a partial release (a run stopped mid-upload, or created by hand with
    // no assets) left partial, or completed by uploading what it already holds.
    for (name, keep) in [("partial", 3), ("empty", 0)] {
        let root = scratch(&format!("publish-{name}"));
        dist(&root, COMMIT);
        let held = root.join("gh/releases").join(TAG);
        std::fs::create_dir_all(&held).expect("release");
        let files = verified(&root);
        let assets: Vec<serde_json::Value> = files[..keep]
            .iter()
            .map(|(n, bytes)| {
                std::fs::write(held.join(n), bytes).expect("asset");
                serde_json::json!({ "name": n })
            })
            .collect();
        answers(
            &root,
            &[(
                format!("repos/{REPO}/releases/tags/{TAG}"),
                serde_json::json!({ "assets": assets }),
            )],
        );
        let (code, out, err) = publish(&root, None);
        assert_eq!(code, Some(0), "{name}: {err}");
        assert!(
            out.ends_with(&format!(
                "release {TAG}: {keep} files already held byte for byte, {} uploaded\n",
                7 - keep
            )),
            "{name}: {out}"
        );
        assert_eq!(released(&root), files, "{name}");
        let args = gh_args(&root);
        assert!(!args.contains("release create"), "{name}: {args}");
        assert_eq!(
            args.contains("release download"),
            keep > 0,
            "{name}: {args}"
        );
    }
}

#[test]
fn publish_refuses_a_held_asset_that_is_not_the_verified_file() {
    // Catches: an asset with other bytes under a verified file's name kept (or
    // replaced, silently), and an asset no verified file names kept beside them; in
    // either case nothing may be uploaded.
    for (name, bytes, want) in [
        (
            format!("kbf-daemon-{COMMIT}-linux-arm64.tar.gz"),
            b"other bytes".as_slice(),
            "with other bytes than the verified file",
        ),
        (
            "kbf-extra.tar.gz".to_owned(),
            b"x".as_slice(),
            "not an asset of",
        ),
    ] {
        let root = scratch("publish-differs");
        dist(&root, COMMIT);
        let held = root.join("gh/releases").join(TAG);
        std::fs::create_dir_all(&held).expect("release");
        std::fs::write(held.join(&name), bytes).expect("asset");
        answers(
            &root,
            &[(
                format!("repos/{REPO}/releases/tags/{TAG}"),
                serde_json::json!({ "assets": [{ "name": name }] }),
            )],
        );
        let (code, _, err) = publish(&root, None);
        assert_eq!(code, Some(1), "{name}");
        assert!(
            err.starts_with(&format!("xtask: release {TAG} holds {name}")),
            "{err}"
        );
        assert!(err.contains(want), "{err}");
        assert_eq!(released(&root), vec![(name.clone(), bytes.to_vec())]);
        assert!(!gh_args(&root).contains("release upload"));
    }
}

#[test]
fn publish_refuses_files_that_fail_the_check_before_asking_github() {
    // Catches: a publish that skips the check of the files it is handed.
    let root = scratch("publish-unchecked");
    dist(&root, OTHER);
    let (code, _, err) = publish(&root, None);
    assert_eq!(code, Some(1));
    assert!(err.contains("not an asset of"), "{err}");
    assert_eq!(gh_args(&root), "");
}

#[test]
fn a_failed_gh_release_fails_the_run() {
    // Catches: a create or upload whose failure is not reported, so the workflow goes
    // green with no release.
    let root = scratch("publish-fails");
    dist(&root, COMMIT);
    let (code, _, err) = publish(&root, Some("release-fails"));
    assert_eq!(code, Some(1));
    assert_eq!(
        err,
        "xtask: gh release create: xtask-fake: release failed\n"
    );
}
