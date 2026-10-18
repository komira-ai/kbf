//! `cargo xtask image-check`'s judgements of what `docker` said about the frontend image
//! (`xtask::image_check`), and the verb's behaviour when `docker` cannot run.

use std::path::Path;

use xtask::image_check::{Ran, judge_inspect, judge_refusal, judge_version, run};

fn ran(code: Option<i32>, stdout: &str) -> Ran {
    Ran {
        code,
        stdout: stdout.to_owned(),
        stderr: String::new(),
    }
}

fn refused(code: Option<i32>, stderr: &str) -> Ran {
    Ran {
        code,
        stdout: String::new(),
        stderr: stderr.to_owned(),
    }
}

const NO_CONFIG: &str = "error: cannot read the config file /etc/kbf/kbf.toml: No such file or directory (os error 2)\n";
const NO_FLAG: &str =
    "error: unexpected argument '--mode' found\n\nUsage: kbf frontend --storage <URL>\n";

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
    assert!(judge_refusal(&refused(Some(2), NO_CONFIG)).is_ok());
    for code in [Some(0), Some(1), Some(75), Some(127), None] {
        assert!(
            judge_refusal(&refused(code, NO_CONFIG)).is_err(),
            "{code:?}"
        );
    }
}

/// Catches the check passing for the wrong reason: a binary built from a tree without
/// `--mode` exits 2 on clap's usage error, and so would a refusal of any other kind.
#[test]
fn exit_2_for_another_reason_is_not_a_refusal() {
    for stderr in [
        NO_FLAG,
        "",
        "error: cannot read the config file /etc/other.toml: gone\n",
        "error: kbf.toml is not valid\n",
    ] {
        let e = judge_refusal(&refused(Some(2), stderr)).expect_err(stderr);
        assert!(e.to_string().contains("another reason"), "{e}");
    }
}

/// The whole verb with a stand-in `docker`: an image whose binary rejects `--mode` is
/// refused as a failed check, and one that refuses for the missing config passes.
#[test]
fn run_refuses_an_image_whose_binary_rejects_mode() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("image-check-fake-docker");
    std::fs::create_dir_all(&dir).expect("dir");
    let script = |name: &str, refusal: &str| {
        let path = dir.join(name);
        std::fs::write(dir.join(format!("{name}.stderr")), refusal).expect("stderr");
        let body = format!(
            "#!/bin/sh\n\
             case \"$1 $4\" in\n\
             'image '*) echo 'sha256:ab 65532:65532';;\n\
             *'--version') echo 'kbf 0.1.0+0123456789ab';;\n\
             *) cat '{}.stderr' >&2; exit 2;;\n\
             esac\n",
            path.display()
        );
        std::fs::write(&path, body).expect("write");
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    };
    let bad = script("rejects-mode", NO_FLAG);
    let e = run(&bad, "img", None).expect_err("a binary that rejects --mode");
    assert!(e.to_string().contains("another reason"), "{e}");
    let good = script("refuses-config", NO_CONFIG);
    assert!(run(&good, "img", None).is_ok());
}

/// Catches a missing program reported as a passed check.
#[test]
fn a_missing_docker_is_an_error() {
    let e = run(Path::new("/nonexistent/docker"), "x", None).expect_err("no docker");
    assert!(e.to_string().contains("/nonexistent/docker"), "{e}");
}
