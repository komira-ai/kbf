//! `cargo xtask sudo-test` as CI runs it: the real binary, with `xtask-fake` as cargo
//! (printing the JSON messages each test writes for it) and as sudo (recording what it
//! was asked to run, and from where).

use kbf_testkit::dir::TestDir;
use std::path::{Path, PathBuf};
use std::process::Command;

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const BUILD: &str = "test --locked --no-run --message-format=json -p kbf-guest --test as_root";

/// One run's directories: the repository root cargo runs in, and the fake sudo's.
struct Run {
    root: PathBuf,
    sudo: PathBuf,
    /// Last field: the directory is removed after the run is over.
    _own: TestDir,
}

impl Run {
    /// An empty root for one test, a fake sudo, and the messages the fake cargo prints:
    /// the target's test binary (`<root>/target/debug/deps/as_root-0f1e`, from the
    /// package at `<root>/crates/kbf-guest`) after `before`.
    fn new(name: &str, before: &str) -> Run {
        let own = TestDir::under(
            Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-sudo-test"),
            name,
        );
        let root = own.to_path_buf();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).expect("scratch");
        let sudo = bin.join("sudo");
        std::fs::hard_link(FAKE, &sudo).expect("link the fake as sudo");
        let package = root.join("crates/kbf-guest");
        std::fs::create_dir_all(&package).expect("package directory");
        let artifact = format!(
            r#"{{"reason":"compiler-artifact","target":{{"kind":["test"],"name":"as_root"}},"profile":{{"test":true}},"executable":"{}","manifest_path":"{}"}}"#,
            root.join("target/debug/deps/as_root-0f1e").display(),
            package.join("Cargo.toml").display()
        );
        std::fs::write(root.join("cargo-messages"), format!("{before}{artifact}\n"))
            .expect("messages");
        Run {
            root,
            sudo,
            _own: own,
        }
    }

    /// `sudo-test -p kbf-guest --test as_root` with `extra` arguments, with the fake
    /// as `cargo` and planting `defect` (or none): exit code and stderr.
    fn xtask(&self, cargo: &Path, defect: Option<&str>, extra: &[&str]) -> (Option<i32>, String) {
        let mut cmd = Command::new(XTASK);
        cmd.arg("--root").arg(&self.root);
        cmd.args(["sudo-test", "-p", "kbf-guest", "--test", "as_root"]);
        cmd.args(extra).env("CARGO", cargo);
        match defect {
            Some(d) => cmd.env("XTASK_FAKE_DEFECT", d),
            None => cmd.env_remove("XTASK_FAKE_DEFECT"),
        };
        let out = cmd.output().expect("run xtask");
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// What the fake cargo was asked to do, one line per run.
    fn cargo_args(&self) -> String {
        std::fs::read_to_string(self.root.join("target/debug/cargo-args")).expect("cargo-args")
    }

    /// What the fake sudo was asked to run, one `<dir>: <args>` line per run; empty
    /// when it never ran.
    fn sudo_args(&self) -> String {
        std::fs::read_to_string(self.sudo.with_file_name("sudo-args")).unwrap_or_default()
    }

    /// The `sudo-args` line of a run of the target's binary with `args`.
    fn ran(&self, args: &str) -> String {
        format!(
            "{}: -n {} --ignored{args}\n",
            self.root.join("crates/kbf-guest").display(),
            self.root.join("target/debug/deps/as_root-0f1e").display()
        )
    }
}

/// Catches the test binary not built as this user from the root (the wrong package or
/// target, a build without `--locked`, no JSON messages), the wrong binary run, a run
/// that prompts for a password (no `-n`), the non-ignored tests run in place of the
/// ignored ones, the arguments after `--` dropped, and the binary run from somewhere
/// other than its package's directory (where `cargo test` runs it). The decoy before
/// the artifact is the package's bin, which is built for the test too.
#[test]
fn the_target_s_binary_runs_as_root_with_ignored_and_the_test_args() {
    let decoy = r#"{"reason":"compiler-artifact","target":{"kind":["bin"],"name":"kbf-guest"},"profile":{"test":false},"executable":"/w/kbf-guest","manifest_path":"/w/Cargo.toml"}"#;
    let run = Run::new("passes", &format!("{decoy}\n"));
    let (code, err) = run.xtask(
        Path::new(FAKE),
        None,
        &[
            "--sudo",
            &run.sudo.display().to_string(),
            "--",
            "--exact",
            "x",
        ],
    );
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(run.cargo_args(), format!("{BUILD}\n"));
    assert_eq!(run.sudo_args(), run.ran(" --exact x"));
}

/// Catches a failed root run passed over: the tests failed as root, so the step fails.
#[test]
fn tests_that_fail_as_root_fail_the_verb() {
    let run = Run::new("tests-fail", "");
    let sudo = run.sudo.display().to_string();
    let (code, err) = run.xtask(Path::new(FAKE), Some("tests-fail"), &["--sudo", &sudo]);
    assert_eq!(code, Some(1));
    assert_eq!(run.sudo_args(), run.ran(""));
    assert!(
        err.ends_with(&format!(
            "xtask: the ignored tests of -p kbf-guest --test as_root failed as root ({}): \
             exit status: 101\n",
            run.root.join("target/debug/deps/as_root-0f1e").display()
        )),
        "{err}"
    );
}

/// Catches a failed build run anyway: sudo must not start when cargo failed, and the
/// step fails on the build.
#[test]
fn a_failed_build_runs_nothing_as_root() {
    let run = Run::new("build-fails", "");
    let sudo = run.sudo.display().to_string();
    let (code, err) = run.xtask(Path::new(FAKE), Some("build-fails"), &["--sudo", &sudo]);
    assert_eq!(code, Some(1));
    assert_eq!(run.sudo_args(), "");
    assert!(
        err.ends_with(
            "xtask: cargo test --no-run -p kbf-guest --test as_root failed: exit status: 101\n"
        ),
        "{err}"
    );
}

/// Catches a build whose messages name no binary for the target (a misspelt
/// `--test`) taken as a pass with nothing run.
#[test]
fn messages_without_the_target_run_nothing_as_root() {
    let run = Run::new("no-target", "");
    let sudo = run.sudo.display().to_string();
    let messages = run.root.join("cargo-messages");
    std::fs::write(
        &messages,
        "{\"reason\":\"build-finished\",\"success\":true}\n",
    )
    .expect("messages");
    let (code, err) = run.xtask(Path::new(FAKE), None, &["--sudo", &sudo]);
    assert_eq!(code, Some(1));
    assert_eq!(run.sudo_args(), "");
    assert!(
        err.ends_with("xtask: cargo built 0 test binaries for the target `as_root`, not 1\n"),
        "{err}"
    );
}

/// Catches a cargo or a sudo that cannot be started reported as anything but that
/// program not starting.
#[test]
fn a_cargo_or_sudo_that_cannot_start_fails_the_verb() {
    let run = Run::new("no-programs", "");
    let missing = run.root.join("bin/missing");
    let sudo = run.sudo.display().to_string();
    let (code, err) = run.xtask(&missing, None, &["--sudo", &sudo]);
    assert_eq!(code, Some(1));
    assert!(err.starts_with("xtask: cargo: "), "{err}");
    let (code, err) = run.xtask(
        Path::new(FAKE),
        None,
        &["--sudo", &missing.display().to_string()],
    );
    assert_eq!(code, Some(1));
    assert!(
        err.starts_with(&format!("xtask: {}: ", missing.display())),
        "{err}"
    );
}
