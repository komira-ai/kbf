//! `cargo xtask image-build`, `image-test` and `image-asset` as the workflow runs them: the
//! real binary, with input each verb refuses. What an image archive must be is the unit tests'
//! (`src/image/tests.rs`); these hold the command lines to the verbs and show that a refusal
//! is an exit status the workflow stops on, said on stderr.

use std::path::Path;
use std::process::Command;

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// The exit code and stderr of `xtask <args>`.
fn xtask(args: &[&str]) -> (Option<i32>, String) {
    let out = Command::new(XTASK).args(args).output().expect("run xtask");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("xtask-image")
        .join(name);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

#[test]
fn a_build_of_a_commit_that_is_not_one_is_refused_before_docker_runs() {
    let out = scratch("build-bad-commit");
    let (code, err) = xtask(&[
        "image-build",
        "--image",
        "frontend",
        "--dockerfile-dir",
        "packaging/container/kbf-frontend",
        "--binary",
        "target/release/kbf",
        "--commit",
        "main",
        "--out",
        &out.to_string_lossy(),
        "--docker",
        "/nonexistent/docker",
    ]);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("not a commit: \"main\""), "{err}");
    assert!(!out.join("kbf-frontend-context").exists());
}

#[test]
fn a_test_of_a_commit_that_is_not_one_is_refused_before_docker_runs() {
    let (code, err) = xtask(&[
        "image-test",
        "--image",
        "coordinator",
        "--archive",
        "kbf-coordinator.tar",
        "--commit",
        "HEAD",
        "--docker",
        "/nonexistent/docker",
    ]);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("not a commit: \"HEAD\""), "{err}");
}

#[test]
fn an_asset_whose_sbom_is_missing_is_refused_and_nothing_is_written() {
    let out = scratch("asset-no-sbom");
    let (code, err) = xtask(&[
        "image-asset",
        "--image",
        "frontend",
        "--archive",
        "kbf-frontend.tar",
        "--binary",
        "target/release/kbf",
        "--sbom",
        &out.join("missing.cdx.json").to_string_lossy(),
        "--platform",
        "linux-x86_64",
        "--commit",
        COMMIT,
        "--out",
        &out.join("dist").to_string_lossy(),
    ]);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("missing.cdx.json"), "{err}");
    assert!(!out.join("dist").exists());
}
