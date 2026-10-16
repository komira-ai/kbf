//! `kbf-coverage` end to end with a relative `--root`, as CONTRIBUTING documents the
//! local command (`tighten --lcov <tracefile> --root .`): `cargo llvm-cov` writes the
//! tracefile's source paths absolute, so the relative root must be resolved against
//! the working directory before the paths are attributed to crates.

use std::path::Path;
use std::process::{Command, Output};

use super::{MATCHING, baseline_of, stderr, stdout, workspace};

/// Rewrites the fixture's tracefile with absolute paths as cargo spells them: under
/// the workspace directory with symbolic links resolved (cargo finds the workspace
/// from the working directory the system reports, which has none).
fn absolute_tracefile(root: &Path) {
    let r = std::fs::canonicalize(root).expect("canonicalize");
    let r = r.display();
    let lcov = format!(
        "SF:{r}/crates/a/src/lib.rs\nLF:4\nLH:4\nBRF:2\nBRH:2\nend_of_record\n\
         SF:{r}/crates/b/src/main.rs\nLF:10\nLH:10\nend_of_record\n"
    );
    std::fs::write(root.join("lcov.info"), lcov).expect("write lcov");
}

fn kbf_coverage(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kbf-coverage"))
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("run kbf-coverage")
}

/// Catches: a relative `--root` joined to the absolute source paths as written, so
/// every record is "not under <root>/crates" and the documented local command exits 2;
/// and a root resolved against something other than the working directory (`..` from
/// a subdirectory names the workspace too). `check` must read the tracefile (a's lines
/// missed 1 -> 0, looser than measured, so it fails with exit 1, not 2), and `tighten`, run exactly as
/// CONTRIBUTING spells it, must lower `a` to the measurement.
#[test]
fn a_relative_root_attributes_absolute_source_paths() {
    let root = workspace("relative-root");
    absolute_tracefile(&root);
    let sub = root.join("crates");
    for (cwd, rel, lcov) in [(&*root, ".", "lcov.info"), (&*sub, "..", "../lcov.info")] {
        std::fs::write(root.join("coverage-baseline"), MATCHING).expect("write baseline");
        let base = if rel == "." {
            "coverage-baseline"
        } else {
            "../coverage-baseline"
        };
        let o = kbf_coverage(cwd, &["--lcov", lcov, "--root", rel, "--baseline", base]);
        assert_eq!(o.status.code(), Some(1), "{rel}: {}", stderr(&o));

        let o = kbf_coverage(cwd, &["tighten", "--lcov", lcov, "--root", rel]);
        assert_eq!(o.status.code(), Some(0), "{rel}: {}", stderr(&o));
        assert_eq!(
            baseline_of(&root),
            "a          0      0\nb          0      -\nempty      -      -\n",
            "{rel}: {}",
            stdout(&o)
        );
    }
}

/// Catches: a relative root that names no directory reported as anything but itself.
#[test]
fn a_relative_root_that_does_not_exist_exits_2() {
    let root = workspace("relative-root-missing");
    std::fs::write(root.join("coverage-baseline"), MATCHING).expect("write baseline");
    let o = kbf_coverage(
        &root,
        &[
            "--lcov",
            "lcov.info",
            "--root",
            "no-such",
            "--baseline",
            "coverage-baseline",
        ],
    );
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("resolve --root no-such:"),
        "{}",
        stderr(&o)
    );
}
