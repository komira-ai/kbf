//! How the last apply stands, in one line of `<work>/status`:
//!
//! ```text
//! running
//! done
//! failed:<reason>[:<what became of the old release>]
//! ```
//!
//! The install verb writes `running` before it launches the apply job; the job writes the
//! end. `query` reports the line as `apply`. A reason is lower case letters and `_`; the
//! tail is `rolled_back` (the previous release is running again), `rollback_failed` or
//! `no_previous` (there was none to go back to).

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;

use super::layout::{APPLY_LABEL, MacHost};
use super::tools;
use crate::runner::Runner;

/// How the last apply stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyState {
    Running,
    Done,
    /// `failed:` and the rest of the line.
    Failed(String),
}

impl ApplyState {
    /// The line, without its end.
    pub fn line(&self) -> String {
        match self {
            Self::Running => "running".to_owned(),
            Self::Done => "done".to_owned(),
            Self::Failed(why) => format!("failed:{why}"),
        }
    }

    /// The state a line says; none for any other text.
    pub fn parse(line: &str) -> Option<Self> {
        match line {
            "running" => Some(Self::Running),
            "done" => Some(Self::Done),
            _ => {
                let why = line.strip_prefix("failed:")?;
                let plain = !why.is_empty()
                    && why.len() <= 64
                    && why
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b == b'_' || b == b':');
                plain.then(|| Self::Failed(why.to_owned()))
            }
        }
    }
}

/// The state in the status file; none when there is none, or it is not a state.
pub fn read(host: &MacHost) -> Option<ApplyState> {
    let text = host.trust.read(&host.status_file()).ok()?;
    ApplyState::parse(text.strip_suffix('\n')?)
}

/// Replace the status file with `state`: written aside and renamed, so a reader sees the
/// old line or the new one.
///
/// # Errors
/// The file could not be written.
pub fn write(host: &MacHost, state: &ApplyState) -> io::Result<()> {
    let aside = host.work.join("status.new");
    let _ = fs::remove_file(&aside);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&aside)?;
    writeln!(file, "{}", state.line())?;
    file.sync_all()?;
    fs::rename(&aside, host.status_file())
}

/// Whether the apply job is loaded and running. A job launchd cannot be asked about counts
/// as running: busy is the safe answer.
fn job_running(host: &MacHost, runner: &dyn Runner) -> bool {
    match runner.run(&tools::print(APPLY_LABEL, host.tuning.tool)) {
        Err(_) => true,
        Ok(exec) => exec.success() && tools::print_running(&exec.stdout_text()),
    }
}

/// The reason an apply is reported as failed when its job is gone.
pub const LOST: &str = "apply_lost";

/// The state to report. A status of `running` whose job is not running is an apply that was
/// lost (the job never started or was killed): it is reported as failed. (An install that is
/// between writing `running` and loading the job holds the install lock; a reader that must
/// not call that a loss asks [`crate::locks::flock_held`] too.)
pub fn effective(host: &MacHost, runner: &dyn Runner) -> Option<ApplyState> {
    match read(host)? {
        ApplyState::Running if !job_running(host, runner) => {
            Some(ApplyState::Failed(LOST.to_owned()))
        }
        state => Some(state),
    }
}
