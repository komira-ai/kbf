//! `cargo xtask image-check`'s judgements of what `docker` said about the frontend image
//! (`xtask::image_check`), and the verb's behaviour when `docker` cannot run.

use std::path::Path;

use xtask::image_check::{Ran, judge_inspect, judge_refusal, judge_version, run};

fn ran(code: Option<i32>, stdout: &str) -> Ran {
    Ran {
        code,
        stdout: stdout.to_owned(),
    }
}

/// Catches a root image, an image whose user is unset (the default is root), a
/// failed inspect read as an id, and an id taken from a differently shaped answer.
#[test]
fn only_the_manifests_uid_passes_inspect() {
    assert_eq!(
        judge_inspect(&ran(Some(0), "sha256:ab 65532:65532\n")).as_deref(),
        Ok("sha256:ab")
    );
    for bad in [
        "sha256:ab \n",
        "sha256:ab root\n",
        "sha256:ab 65532\n",
        "sha256:ab",
    ] {
        assert!(judge_inspect(&ran(Some(0), bad)).is_err(), "{bad:?}");
    }
    assert!(judge_inspect(&ran(Some(1), "sha256:ab 65532:65532")).is_err());
}

/// Catches a binary that fails to start (a missing library exits 127) passed, and a
/// `--version` that is not kbf's stamp (no commit, or another program's output).
#[test]
fn the_version_must_start_and_name_a_commit() {
    assert!(judge_version(&ran(Some(0), "kbf 0.1.0+0123456789ab\n")).is_ok());
    assert!(judge_version(&ran(Some(127), "kbf 0.1.0+0123456789ab\n")).is_err());
    for bad in [
        "",
        "kbf 0.1.0\n",
        "kbf 0.1.0+\n",
        "kbf +abc\n",
        "other 1+2\n",
    ] {
        assert!(judge_version(&ran(Some(0), bad)).is_err(), "{bad:?}");
    }
}

/// Catches a frontend that starts (0), dies on a signal (None) or fails on a missing
/// library (127, or 1) all read as "refused for lack of a config".
#[test]
fn only_exit_2_is_a_refusal() {
    assert!(judge_refusal(&ran(Some(2), "")).is_ok());
    for code in [Some(0), Some(1), Some(75), Some(127), None] {
        assert!(judge_refusal(&ran(code, "")).is_err(), "{code:?}");
    }
}

/// Catches a missing program reported as a passed check.
#[test]
fn a_missing_docker_is_an_error() {
    let e = run(Path::new("/nonexistent/docker"), "x", None).expect_err("no docker");
    assert!(e.to_string().contains("/nonexistent/docker"), "{e}");
}
