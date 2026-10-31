//! Running one step of a hook as a transient systemd unit with exactly the declared rights.
//!
//! The command line is a pure function of the declaration, the step and the generation
//! ([`command`]); starting it sits behind [`Spawner`], so a test runs the whole of
//! [`run_step`] against a fake. The properties are those of docs/design/host-updates.md,
//! section 3.4:
//!
//! | declaration | property |
//! |---|---|
//! | `hook.user`, `hook.group` | `User=`, `Group=` |
//! | the step's capabilities | `AmbientCapabilities=` and `CapabilityBoundingSet=`, exactly that list; `NoNewPrivileges=yes` |
//! | `read_paths`, `write_paths`, `state_dir` | `ProtectSystem=strict`, `ProtectHome=yes`, `PrivateTmp=yes`, `ReadOnlyPaths=`, `ReadWritePaths=` (the writable paths and the record directory) |
//! | `address_families` | `RestrictAddressFamilies=`, `none` for an empty list |
//! | `environment` | one `Environment=` per variable, and nothing inherited |
//! | `timeout_secs.<step>` | `RuntimeMaxSec=` |
//!
//! `ProtectSystem=strict` makes the whole tree read-only, so what stays writable is exactly
//! `ReadWritePaths=` and the API file systems; it also leaves the tree readable, so
//! `read_paths` says what the hook is declared to read and does not hide the rest.
//!
//! The unit is named `updater-<service>-<step>-<generation>`, so a resumed step can find and
//! stop the unit of the run it replaces. `--wait --collect --pipe` makes `systemd-run` wait
//! for the unit, remove it even when it failed, carry the hook's standard output through and
//! exit with the hook's exit status.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use update_hook::{Outcome, Report, Step};

use crate::declaration::Declaration;

/// Where `systemd-run` is.
pub const SYSTEMD_RUN: &str = "/usr/bin/systemd-run";

/// The properties every step has, whatever the declaration says.
pub const FIXED_PROPERTIES: [&str; 10] = [
    "ProtectKernelTunables=yes",
    "ProtectKernelModules=yes",
    "ProtectKernelLogs=yes",
    "ProtectControlGroups=yes",
    "RestrictNamespaces=yes",
    "RestrictRealtime=yes",
    "LockPersonality=yes",
    "MemoryDenyWriteExecute=yes",
    "SystemCallFilter=@system-service",
    "SystemCallArchitectures=native",
];

/// The unit that runs `step` of `service` for one order. The generation counts the runs of the
/// order, so a rerun after a crash has a name of its own and the old one can be found.
pub fn unit_name(_service: &str, _step: Step, _generation: u64) -> String {
    String::new()
}

/// One run of one step.
#[derive(Clone, Copy, Debug)]
pub struct Launch<'a> {
    pub declaration: &'a Declaration,
    pub step: Step,
    pub generation: u64,
    /// The staged release tree: the hook is `release/<binary>`, and it is the argument of
    /// `prepare`.
    pub release: &'a Path,
    /// The file of the `[hook.params]` table, if the step is given one.
    pub params: Option<&'a Path>,
}

/// Why a step could not be started.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LaunchError {
    #[error("the staged release {0:?} is not an absolute path")]
    RelativeRelease(PathBuf),
    #[error("the params file {0:?} is not an absolute path")]
    RelativeParams(PathBuf),
}

/// The arguments of `systemd-run`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemdRun {
    pub args: Vec<OsString>,
}

/// The `Name=value` properties of a step, in the order they are given.
pub fn properties(_declaration: &Declaration, _step: Step) -> Vec<String> {
    Vec::new()
}

/// The `systemd-run` command that runs one step.
pub fn command(_launch: &Launch<'_>) -> Result<SystemdRun, LaunchError> {
    Ok(SystemdRun { args: Vec::new() })
}

/// What `systemd-run` did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finished {
    /// The exit status; `None` if `systemd-run` was killed by a signal.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Starts `systemd-run` and waits for it.
pub trait Spawner {
    fn run(&self, command: &SystemdRun) -> io::Result<Finished>;
}

/// The real thing: runs a program (`systemd-run`) with an empty environment and no input.
#[derive(Clone, Debug)]
pub struct Systemd {
    program: PathBuf,
}

impl Systemd {
    pub fn new(program: &Path) -> Systemd {
        Systemd {
            program: program.to_owned(),
        }
    }

    /// `/usr/bin/systemd-run`.
    pub fn system() -> Systemd {
        Systemd::new(Path::new(SYSTEMD_RUN))
    }
}

impl Spawner for Systemd {
    fn run(&self, _command: &SystemdRun) -> io::Result<Finished> {
        let _ = &self.program;
        Err(io::ErrorKind::Unsupported.into())
    }
}

/// How a step ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepRun {
    pub unit: String,
    pub outcome: Outcome,
    /// The last line of the hook's standard output, read as the updater reads it.
    pub report: Report,
    /// The hook's standard error, for the log.
    pub stderr: String,
}

/// Why a step did not run to an end.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Launch(#[from] LaunchError),
    #[error("systemd-run could not be started: {0}")]
    Spawn(#[from] io::Error),
}

/// Runs one step and reads how it ended: the exit status as an [`Outcome`] (a status of no
/// contract, a signal and a timeout are failures) and the last line of standard output as a
/// [`Report`] (a missing or malformed line is `{}`).
pub fn run_step(_spawner: &dyn Spawner, _launch: &Launch<'_>) -> Result<StepRun, RunError> {
    Err(RunError::Spawn(io::ErrorKind::Unsupported.into()))
}
