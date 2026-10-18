//! What the `kbf-installer` binary prints for `--version` and `--version --json`, run as
//! the artifacts workflow runs it (`cargo xtask build-commit check --json`, and
//! `check-asset` on the tarball): the commit and the profile it was built with, read from
//! the checkout here and not from the library.

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_kbf-installer");

/// The exit code, stdout and stderr of `kbf-installer <args>`.
fn run(args: &[&str]) -> (Option<i32>, String, String) {
    let out = Command::new(BIN).args(args).output().expect("runs");
    (
        out.status.code(),
        String::from_utf8(out.stdout).expect("UTF-8"),
        String::from_utf8(out.stderr).expect("UTF-8"),
    )
}

/// The commit this binary must report: the first 12 digits of the checkout's HEAD, which
/// the build script embeds.
fn head() -> String {
    let head = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .expect("git runs");
    assert!(head.status.success(), "the tests run in a git checkout");
    String::from_utf8(head.stdout)
        .expect("UTF-8")
        .trim()
        .to_owned()
}

/// Catches a `kbf-installer --version` that names the package version alone, or another
/// name: the artifacts workflow's `build-commit check` wants `kbf-installer <package
/// version>+<commit>`, so that two builds of one package version read differently and a
/// binary built from another commit is refused.
#[test]
fn version_names_the_commit_built() {
    for flag in ["--version", "-V"] {
        let (code, out, err) = run(&[flag]);
        assert_eq!(code, Some(0), "{flag}: {err}");
        assert_eq!(
            out,
            format!("kbf-installer {}+{}\n", env!("CARGO_PKG_VERSION"), head()),
            "{flag}"
        );
    }
}

/// Catches a `kbf-installer --version --json` that is not one JSON object on one line of
/// stdout, that lacks or adds a field (a host and the workflow read these to check what
/// they run, so the set is the contract), that reports another commit than the checkout's
/// or the package version, or that calls a debug build a release (or the reverse).
#[test]
fn version_json_reports_the_build_identity() {
    for argv in [["--version", "--json"], ["--json", "--version"]] {
        let (code, out, err) = run(&argv);
        assert_eq!(code, Some(0), "{argv:?}: {err}");
        assert!(err.is_empty(), "{err}");
        assert_eq!(out.lines().count(), 1, "{out}");
        assert!(out.ends_with('\n'), "{out:?}");
        let v: serde_json::Value = serde_json::from_str(&out).expect("one JSON value");
        let fields: Vec<&str> = v
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(fields, ["commit", "profile", "version"], "{out}");
        assert_eq!(v["commit"], head());
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        let profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };
        assert_eq!(v["profile"], profile);
    }
}
