//! Kill points: the places where the harness stops a step to test crash-resume.
//!
//! A fake service (or a hook, for a test that wants one) calls [`point`] before each change it
//! makes. Under the harness, the environment names a directory and the number of the point to
//! stop at; each call appends to a count file there, and the call that is the target announces
//! itself on standard error and parks, to be killed. The harness waits for the announcement,
//! never for a time, and kills the step's whole process group, so a hook that is a script
//! calling a fake binary is stopped as a whole. Outside the harness (no variables) a call does
//! nothing.
//!
//! The count of a complete run tells the harness how many points a step has.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

/// Names the directory of the count file.
pub const GATE_DIR_VAR: &str = "UPDATE_HOOK_GATE_DIR";
/// Names the point to stop at (counted from 1; `0` or absent: none).
pub const KILL_AT_VAR: &str = "UPDATE_HOOK_KILL_AT";
/// The line a parked step writes to standard error.
pub const PARKED: &str = "update-hook-gate: parked at the kill point";
/// The count file in the gate directory: one byte per point reached.
pub(crate) const COUNT_FILE: &str = "gate-count";

/// The harness's instructions to the step it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gate {
    dir: PathBuf,
    kill_at: u64,
}

impl Gate {
    /// From the values of the two variables; `None` without a directory. A missing or unreadable
    /// number is `0`: count, never stop.
    pub fn parse(dir: Option<OsString>, kill_at: Option<OsString>) -> Option<Gate> {
        let dir = PathBuf::from(dir?);
        let kill_at = kill_at
            .and_then(|value| value.to_str()?.parse().ok())
            .unwrap_or(0);
        Some(Gate { dir, kill_at })
    }

    /// From this process's environment.
    pub fn from_env() -> Option<Gate> {
        Gate::parse(
            std::env::var_os(GATE_DIR_VAR),
            std::env::var_os(KILL_AT_VAR),
        )
    }

    /// Counts one point and returns its number, `0` if the count cannot be kept (the harness
    /// then sees no points and refuses the hook).
    pub fn count(&self) -> u64 {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(COUNT_FILE))
            .and_then(|mut file| file.write_all(b".").map(|()| file))
            .and_then(|file| file.metadata())
            .map_or(0, |meta| meta.len())
    }

    /// Counts a point and calls `stop` if it is the target.
    pub fn reach_then(&self, stop: &dyn Fn()) {
        if self.count() == self.kill_at {
            stop();
        }
    }
}

/// Writes [`PARKED`] to standard error.
pub fn announce_parked() {
    eprintln!("{PARKED}");
}

/// Announces with `announce` and parks, again after every wake-up: the harness kills the process
/// at the first announcement.
pub fn park_with(announce: &dyn Fn()) {
    loop {
        announce();
        std::thread::park();
    }
}

/// A kill point. Call it before each change a fake service makes to the world or a hook makes
/// to its record.
pub fn point() {
    if let Some(gate) = Gate::from_env() {
        gate.reach_then(&|| park_with(&announce_parked));
    }
}
