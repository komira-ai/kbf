//! `cargo xtask proxy-loom`: the loom models of kbf-proxy's slot and gate.
//!
//! The models are the `loom_*` unit tests of `crates/kbf-proxy/src/slot/loom_tests.rs`. They
//! are built only under `--cfg kbf_loom`, which swaps the atomics `slot.rs` takes for loom's
//! (`crate::sync`); the cfg is not `loom`, because tokio reads that one too. The run builds
//! in its own target directory, so the cfg does not rebuild the ordinary one, and in release
//! mode: the models walk every interleaving and run an order of magnitude faster.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::{Command, Output};

use crate::Error;

/// The cfg that selects the models.
const CFG: &str = "--cfg kbf_loom";

/// The command that runs the models from `root`, under the `RUSTFLAGS` the caller had
/// (`rustflags`), which keeps CI's `-D warnings` on the models' build too.
pub fn command(cargo: &OsStr, root: &Path, rustflags: Option<&OsStr>) -> Command {
    let mut flags = rustflags.map(OsStr::to_owned).unwrap_or_default();
    if !flags.is_empty() {
        flags.push(" ");
    }
    flags.push(CFG);
    let mut command = Command::new(cargo);
    command
        .current_dir(root)
        .env("RUSTFLAGS", flags)
        .env("CARGO_TARGET_DIR", root.join("target").join("loom"))
        .args(["test", "-p", "kbf-proxy", "--lib", "--release", "--locked"])
        .args(["--", "loom_"]);
    command
}

/// The fewest models a passing run must have run. A filter that matches nothing exits 0 with
/// "running 0 tests", so renaming the models would turn the gate off without this count.
pub const MIN_MODELS: usize = 5;

/// How many tests cargo's output says passed: the sum of every `N passed` of its
/// `test result:` lines.
fn passed_count(output: &str) -> usize {
    output
        .lines()
        .filter(|line| line.starts_with("test result: ok."))
        .filter_map(|line| {
            let (before, _) = line.split_once(" passed")?;
            before.rsplit(' ').next()?.parse::<usize>().ok()
        })
        .sum()
}

/// Runs the models: the lines to print when they pass.
///
/// # Errors
/// Cargo could not be started, a model failed (cargo's own output says which), or fewer than
/// [`MIN_MODELS`] models ran.
pub fn run(cargo: &OsStr, root: &Path) -> Result<Vec<String>, Error> {
    let rustflags: Option<OsString> = std::env::var_os("RUSTFLAGS");
    let output = command(cargo, root, rustflags.as_deref())
        .output()
        .map_err(Error::spawn("cargo".to_owned()))?;
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    report(&output)
}

/// What a finished run means: passed with enough models, or why not.
fn report(output: &Output) -> Result<Vec<String>, Error> {
    if !output.status.success() {
        return Err(Error::Failed(format!(
            "the kbf-proxy loom models failed: {}",
            output.status
        )));
    }
    let ran = passed_count(&String::from_utf8_lossy(&output.stdout));
    if ran < MIN_MODELS {
        return Err(Error::Failed(format!(
            "the kbf-proxy loom models ran {ran} tests, fewer than {MIN_MODELS}: \
             were they renamed, or is the filter wrong?"
        )));
    }
    Ok(vec![format!(
        "the kbf-proxy loom models passed ({ran} ran)"
    )])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn env(command: &Command, name: &str) -> Option<String> {
        command
            .get_envs()
            .find(|(k, _)| *k == name)
            .and_then(|(_, v)| v)
            .map(|v| v.to_string_lossy().into_owned())
    }

    #[test]
    fn the_models_run_in_release_under_the_cfg_in_their_own_target_dir() {
        let command = command(OsStr::new("cargo"), Path::new("/repo"), None);
        assert_eq!(
            args(&command),
            [
                "test",
                "-p",
                "kbf-proxy",
                "--lib",
                "--release",
                "--locked",
                "--",
                "loom_"
            ]
        );
        assert_eq!(
            env(&command, "RUSTFLAGS").as_deref(),
            Some("--cfg kbf_loom")
        );
        assert_eq!(
            env(&command, "CARGO_TARGET_DIR").as_deref(),
            Some("/repo/target/loom")
        );
        assert_eq!(command.get_current_dir(), Some(Path::new("/repo")));
    }

    #[test]
    fn the_callers_rustflags_stay_in_front_of_the_cfg() {
        let command = command(
            OsStr::new("cargo"),
            Path::new("/repo"),
            Some(OsStr::new("-D warnings")),
        );
        assert_eq!(
            env(&command, "RUSTFLAGS").as_deref(),
            Some("-D warnings --cfg kbf_loom")
        );
        let empty = self::command(
            OsStr::new("cargo"),
            Path::new("/repo"),
            Some(OsStr::new("")),
        );
        assert_eq!(env(&empty, "RUSTFLAGS").as_deref(), Some("--cfg kbf_loom"));
    }

    #[test]
    fn the_passed_count_sums_every_result_line() {
        let output = "running 5 tests\ntest a ... ok\n\
            test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 90 filtered out\n\
            test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n";
        assert_eq!(passed_count(output), 7);
        assert_eq!(
            passed_count("running 0 tests\ntest result: ok. 0 passed; 0 failed\n"),
            0
        );
        assert_eq!(passed_count("test result: FAILED. 4 passed; 1 failed\n"), 0);
        assert_eq!(passed_count(""), 0);
    }

    fn finished(code: i32, stdout: &str) -> Output {
        use std::os::unix::process::ExitStatusExt;
        Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    #[test]
    fn a_run_passes_only_with_enough_models_and_a_clean_exit() {
        let five = "test result: ok. 5 passed; 0 failed; 0 ignored\n";
        let ok = report(&finished(0, five)).expect("five models ran");
        assert_eq!(ok, ["the kbf-proxy loom models passed (5 ran)"]);
        let four = report(&finished(0, "test result: ok. 4 passed; 0 failed\n"))
            .expect_err("four is fewer than the gate needs");
        assert!(four.to_string().contains("ran 4 tests"), "{four}");
        let none = report(&finished(0, "running 0 tests\n")).expect_err("no model ran");
        assert!(none.to_string().contains("ran 0 tests"), "{none}");
        let failed = report(&finished(101, five)).expect_err("a failing exit");
        assert!(
            failed.to_string().contains("loom models failed"),
            "{failed}"
        );
    }

    #[test]
    fn a_failing_or_missing_cargo_is_an_error() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        // `true` exits 0 and prints nothing: the gate that ran nothing.
        let none = run(OsStr::new("true"), root).expect_err("no model ran");
        assert!(none.to_string().contains("ran 0 tests"), "{none}");
        let failed = run(OsStr::new("false"), root).expect_err("false fails");
        assert!(
            failed.to_string().contains("loom models failed"),
            "{failed}"
        );
        let missing = run(OsStr::new("/no/such/cargo"), root).expect_err("cannot start");
        assert!(matches!(missing, Error::Spawn { .. }), "{missing}");
    }
}
