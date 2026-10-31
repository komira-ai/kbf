//! What the runner needs from whatever runs one step of a hook with the rights of its
//! declaration (migration step 3 implements it with transient systemd units). The runner
//! decides which step runs next, with which executable, from which release and by when; the
//! launcher decides how: the user, the capabilities, the paths and the units of the declaration.

use std::io;
use std::path::Path;

use update_hook::Invocation;

/// One run of one step.
#[derive(Clone, Debug)]
pub struct StepCall<'a> {
    /// The service, whose declaration the launcher runs the step under.
    pub service: &'a str,
    /// The step and its arguments, in the form `Invocation::to_args` prints: `prepare` carries
    /// the staged release as its `<artifact>`, every step carries `--params <file>`.
    pub invocation: Invocation,
    /// The hook executable: a file of the verified release the runner checked with
    /// `Staged::check` just before (see its guarantees), the previous release's only for the
    /// `rollback` fallback. The launcher executes this path as it is.
    pub executable: &'a Path,
    /// The deterministic name of the transient unit: `updater-<service>-<step>-<generation>`.
    /// A launcher that runs the step as a unit uses it; it is stopped by name on resume.
    pub unit: &'a str,
    /// The bound of this step, in seconds: the declaration's `timeout_secs` for the step, or
    /// the time left of the verify window when that is shorter.
    pub timeout_secs: u64,
}

/// How a step ended, as the launcher saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepExit {
    /// The exit code; `None` when the step was killed by its bound or by a signal.
    pub code: Option<i32>,
    /// The last line the hook wrote to standard output, `None` when it wrote none.
    pub last_line: Option<String>,
}

/// Why a step could not be run to an end.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    /// The hook could not be started (the executable is missing or cannot be executed). The
    /// runner reads it as a failed step, and as the one reason to try the previous release's
    /// hook for `rollback`.
    #[error("the hook did not start: {0}")]
    NoStart(String),
    /// The launcher itself failed (no unit could be made, the unit could not be read): the
    /// step is left `running` in the record and the runner stops with this error. Resume stops
    /// the unit by name and runs the step again.
    #[error("the launcher failed: {0}")]
    Launcher(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Runs steps. One launcher is shared by the runner and the dead-man, which stops a step from
/// its own thread.
pub trait StepLauncher: Send + Sync {
    /// Runs the step and waits for it to end. A step killed by its bound is a [`StepExit`] with
    /// no code.
    fn run(&self, call: &StepCall<'_>) -> Result<StepExit, LaunchError>;

    /// Stops the unit of that name and waits until it has ended; a unit that is not running is
    /// not an error.
    fn stop(&self, unit: &str) -> Result<(), LaunchError>;
}
