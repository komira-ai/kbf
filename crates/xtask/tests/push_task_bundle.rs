//! `cargo xtask push-task-bundle` run as the release job runs it: the real binary, its flags
//! parsed and the verb dispatched. Each case is refused before any request is sent (no
//! network): the flags, the repository, the credential, the asset, and a tarball that is not a
//! layout; and the summary is left as it was.

use std::path::Path;
use std::process::Command;

use kbf_testkit::dir::TestDir;

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const REPOSITORY: &str = "ghcr.io/komira-ai/kbf-task";

/// An empty directory of its own for one test.
fn scratch(name: &str) -> TestDir {
    TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-push"),
        name,
    )
}

/// The verb with `asset`, `repository`, the tag `v0.0.116` and the summary `dir/summary.md`,
/// the token `token` (none: the variable removed), and the flag `drop` left out when named.
fn push(
    dir: &Path,
    asset: &Path,
    repository: &str,
    token: Option<&str>,
    drop: Option<&str>,
) -> (Option<i32>, String, String) {
    let summary = dir.join("summary.md");
    let asset = asset.display().to_string();
    let summary = summary.display().to_string();
    let flags = [
        ("--asset", asset.as_str()),
        ("--repository", repository),
        ("--tag", "v0.0.116"),
        ("--summary", summary.as_str()),
    ];
    let mut cmd = Command::new(XTASK);
    cmd.arg("push-task-bundle");
    for (flag, value) in flags.into_iter().filter(|(flag, _)| Some(*flag) != drop) {
        cmd.arg(flag).arg(value);
    }
    match token {
        Some(token) => cmd.env("GH_TOKEN", token),
        None => cmd.env_remove("GH_TOKEN"),
    };
    let out = cmd.output().expect("run xtask");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// An asset in `dir` holding `bytes`.
fn asset(dir: &Path, bytes: &[u8]) -> std::path::PathBuf {
    let path = dir.join("kbf-task-bundle-c0ffee-darwin-arm64.tar.gz");
    std::fs::write(&path, bytes).expect("write the asset");
    path
}

#[test]
fn a_flag_left_out_is_a_usage_error() {
    // Catches: the verb not wired into the CLI, or a flag the release job passes renamed or made
    // optional: each of the four is required by name.
    let dir = scratch("flags");
    let file = asset(&dir, b"");
    for flag in ["--asset", "--repository", "--tag", "--summary"] {
        let (code, out, err) = push(&dir, &file, REPOSITORY, Some("t"), Some(flag));
        assert_eq!(code, Some(2), "{flag}: {out}{err}");
        assert!(
            err.contains("the following required arguments were not provided"),
            "{flag}: {err}"
        );
        assert!(err.contains(flag), "{flag}: {err}");
    }
}

#[test]
fn a_repository_with_a_scheme_is_refused_by_the_verb() {
    // Catches: the CLI's repository passed on unchecked, to be read as a host named `https:`.
    let dir = scratch("scheme");
    let file = asset(&dir, b"");
    let (code, out, err) = push(
        &dir,
        &file,
        "https://ghcr.io/komira-ai/kbf-task",
        Some("t"),
        None,
    );
    assert_eq!(code, Some(1), "{out}{err}");
    assert_eq!(
        err,
        "xtask: --repository \"https://ghcr.io/komira-ai/kbf-task\": expected lower-case \
         <host>/<path>, with no scheme and no empty part\n"
    );
    assert!(out.is_empty(), "{out}");
}

#[test]
fn the_verb_with_no_token_is_refused_and_names_the_variable() {
    // Catches: a push attempted with no credential, or a credential taken from another variable
    // than the one the release job sets.
    let dir = scratch("token");
    let file = asset(&dir, b"");
    for token in [None, Some("")] {
        let (code, out, err) = push(&dir, &file, REPOSITORY, token, None);
        assert_eq!(code, Some(1), "{token:?}: {out}{err}");
        assert_eq!(
            err,
            "xtask: GH_TOKEN is not set (or is empty): the registry credential\n"
        );
    }
    assert!(!dir.join("summary.md").exists(), "no summary for a refusal");
}

#[test]
fn an_asset_that_is_missing_or_not_a_layout_is_refused_and_the_token_never_shown() {
    // Catches: a missing asset read as an empty bundle, a tarball that is not one read as a
    // layout, and the credential echoed in an error.
    let dir = scratch("asset");
    let token = "secret-token-value";
    let missing = dir.join("missing.tar.gz");
    let (code, _, err) = push(&dir, &missing, REPOSITORY, Some(token), None);
    assert_eq!(code, Some(1), "{err}");
    assert_eq!(
        err,
        format!(
            "xtask: read {}: No such file or directory (os error 2)\n",
            missing.display()
        )
    );
    let file = asset(&dir, b"not a gzip stream");
    let (code, out, err) = push(&dir, &file, REPOSITORY, Some(token), None);
    assert_eq!(code, Some(1), "{out}{err}");
    assert!(
        err.starts_with("xtask: the asset is not a layout tarball: not a gzip stream: "),
        "{err}"
    );
    assert!(!err.contains(token) && !out.contains(token), "{err}");
    assert!(!dir.join("summary.md").exists(), "no summary for a refusal");
}
