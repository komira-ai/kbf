//! `kbf package-hook <step>`: the update hook of the native kbf package.
//!
//! hostkeeper runs `/usr/bin/kbf package-hook <step> [<staged release dir>] --params <file>` as
//! root around an update of the `kbf` package. The steps keep the hook contract of
//! [`update_hook`] and drive one systemd unit ([`RUN_UNIT`]):
//!
//! | step | what it does |
//! |---|---|
//! | `prepare <staged>` | refuses a staged path that is not `updater.staged_release`; runs the staged `kbf --mode=<mode> --check` (0 and 75 pass, 2 is `config_refused`, anything else `environment`); records whether the unit was active; stops the unit, which drains (kbf finishes what is in flight on SIGTERM and the unit waits) |
//! | `cutover` | starts the unit (the package manager has installed the new binary since `prepare`) |
//! | `verify` | passes when the unit is active and `kbf --version --json` names `updater.target_version`; `activating` is exit 75 `not_active_yet`; anything else fails |
//! | `rollback` | the package manager has put the previous package back: restarts the unit if it was active before this update, stops it on a first install or when it was stopped before |
//! | `cleanup` | refuses unless `verify` succeeded; removes the record |
//!
//! Every step decides from its durable record in `state_dir` ([`record`]) and holds an
//! exclusive lock on that directory for the whole step: a second copy exits 1 and changes
//! nothing. The last line is a [`update_hook::Report`]; the log goes to standard error.
//!
//! The manifest constants the release tooling writes into the desired state are defined here.

pub mod fake;
#[cfg(test)]
mod fake_tests;
mod host;
pub mod record;
mod run;
#[cfg(test)]
mod run_tests;
mod steps;
#[cfg(test)]
mod steps_tests;
mod systemd;
#[cfg(test)]
mod systemd_tests;
#[cfg(test)]
mod testing;
#[cfg(test)]
mod value_tests;

pub use host::{Check, Host, HostError, UnitState};
pub use run::{Settings, main, run};
pub use steps::Done;
pub use systemd::{Systemd, parse_version_output};

/// The command hostkeeper runs: `HOOK_COMMAND <step> ...`.
pub const HOOK_COMMAND: [&str; 2] = ["/usr/bin/kbf", "package-hook"];
/// The package name.
pub const PACKAGE: &str = "kbf";
/// The systemd unit the package runs.
pub const RUN_UNIT: &str = "kbf-storage-and-daemon.service";
/// The `--mode` the staged binary is checked in.
pub const MODE: &str = "storage-and-daemon";
/// The hook's record directory.
pub const STATE_DIR: &str = "/var/lib/kbf-package-hook";
/// The keys of the manifest's `[params]`.
pub const PARAM_KEYS: [&str; 3] = ["mode", "state_dir", "unit"];
/// Seconds `prepare` may take: 0 is unbounded, the drain waits however long in-flight work takes.
pub const PREPARE_SECS: u64 = 0;
/// Seconds `cutover` may take.
pub const CUTOVER_SECS: u64 = 600;
/// Seconds `verify` may take.
pub const VERIFY_SECS: u64 = 360;
/// Seconds `rollback` may take.
pub const ROLLBACK_SECS: u64 = 900;
/// Seconds `cleanup` may take.
pub const CLEANUP_SECS: u64 = 120;
/// How long `verify` may be deferred, in seconds: at least `VERIFY_SECS + ROLLBACK_SECS`.
pub const VERIFY_DEADLINE_SECS: u64 = 1500;
/// How often a failed `cleanup` is run again.
pub const CLEANUP_RETRIES: u32 = 3;

/// Why a version could not be made a native package version.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum VersionError {
    #[error("version {0:?} is not three dot-separated decimal numbers")]
    NotThreeNumbers(String),
    #[error("kbf --version --json did not name a version and a seq: {0}")]
    BadOutput(String),
}

/// The Debian version of the native package: `<major>.<minor>.<seq>`, exactly three decimal
/// numbers, as the shared apt repository requires, and growing along main with `seq`.
///
/// # Errors
/// `version` is not three dot-separated decimal numbers.
pub fn native_version(version: &str, seq: u64) -> Result<String, VersionError> {
    let mut parts = version.split('.').map(|part| {
        let digits = !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
        digits.then(|| part.parse::<u64>().ok()).flatten()
    });
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(Some(major)), Some(Some(minor)), Some(Some(_)), None) => {
            Ok(format!("{major}.{minor}.{seq}"))
        }
        _ => Err(VersionError::NotThreeNumbers(version.to_owned())),
    }
}
