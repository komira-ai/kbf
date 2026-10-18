//! `cargo xtask proxy-loom`: the loom models of kbf-proxy's slot and gate.
//!
//! The models are the `loom_*` unit tests of `crates/kbf-proxy/src/slot/loom_tests.rs`. They
//! are built only under `--cfg kbf_loom`, which swaps the atomics `slot.rs` takes for loom's
//! (`crate::sync`); the cfg is not `loom`, because tokio reads that one too. The run builds
//! in its own target directory, so the cfg does not rebuild the ordinary one, and in release
//! mode: the models walk every interleaving and run an order of magnitude faster.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Command;

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

/// Runs the models: the lines to print when they pass.
///
/// # Errors
/// Cargo could not be started, or a model failed (cargo's own output says which).
pub fn run(cargo: &OsStr, root: &Path) -> Result<Vec<String>, Error> {
    let rustflags: Option<OsString> = std::env::var_os("RUSTFLAGS");
    let status = command(cargo, root, rustflags.as_deref())
        .status()
        .map_err(Error::spawn("cargo".to_owned()))?;
    if status.success() {
        Ok(vec!["the kbf-proxy loom models passed".to_owned()])
    } else {
        Err(Error::Failed(format!(
            "the kbf-proxy loom models failed: {status}"
        )))
    }
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
    fn a_passing_run_says_so_and_a_failing_or_missing_cargo_is_an_error() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let ok = run(OsStr::new("true"), root).expect("true passes");
        assert_eq!(ok, ["the kbf-proxy loom models passed"]);
        let failed = run(OsStr::new("false"), root).expect_err("false fails");
        assert!(
            failed.to_string().contains("loom models failed"),
            "{failed}"
        );
        let missing = run(OsStr::new("/no/such/cargo"), root).expect_err("cannot start");
        assert!(matches!(missing, Error::Spawn { .. }), "{missing}");
    }
}
