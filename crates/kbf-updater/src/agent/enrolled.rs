//! An agent started before its host is enrolled: the packaged unit starts the agent at boot with
//! `--state-dir`, and `kbf-updater enroll` (`packaging/host-agent/README.md`, "Enrolling a host")
//! may not have filled that directory yet.
//!
//! Such an agent does not fail and restart in a loop. It says once what it waits for, calls
//! nothing (no coordinator, no frontend, no helper), and looks at the directory again after a
//! nap. The enrollment is there once `enroll` has committed it (`node.json` is the last file it
//! moves into place, [`kbf_enroll::store`]); the agent then starts as it would have if the files
//! had been there from the start. A directory whose enrollment cannot be read is not waited for:
//! it is refused with the reason, as a start that fails.
//!
//! An agent given files an operator placed (`--client-cert`, `--client-key`, `--ca`) has no
//! enrollment to wait for and starts at once.

use std::path::Path;
use std::time::Duration;

use kbf_enroll::store::Store;

use super::run::Stop;

/// How long an agent that is not enrolled naps between two looks at its state directory.
pub const LOOK_EVERY: Duration = Duration::from_secs(5);

/// How an agent that is not enrolled waits.
pub struct Waiting<'a> {
    /// Called between two looks; returns when the next look is due, or at once when `stop` is
    /// asked.
    pub nap: &'a dyn Fn(&Stop),
    /// Where the one line that says what the agent waits for goes.
    pub log: &'a dyn Fn(&str),
}

/// How a wait ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Waited {
    /// The host is enrolled: the agent starts.
    Enrolled,
    /// The agent was asked to stop first.
    Stopped,
}

/// The line an agent that is not enrolled says, once.
#[must_use]
pub fn not_enrolled(state_dir: &Path) -> String {
    format!(
        "not enrolled: {dir} holds no enrollment yet; waiting for `kbf-updater enroll --state-dir \
         {dir}`, and starting once it has run",
        dir = state_dir.display()
    )
}

/// Waits until `state_dir` holds an enrollment, or `stop` is asked (seen after a nap, which a stop
/// cuts short). Says [`not_enrolled`] once, before the first nap, and nothing when the host is
/// enrolled already.
///
/// # Errors
/// The enrollment cannot be read (the directory is not the agent's, or `node.json` is damaged).
pub fn wait(state_dir: &Path, stop: &Stop, waiting: &Waiting) -> Result<Waited, String> {
    let store = Store::unopened(state_dir);
    let mut said = false;
    loop {
        if store.load().map_err(|e| e.to_string())?.is_some() {
            return Ok(Waited::Enrolled);
        }
        if !said {
            (waiting.log)(&not_enrolled(state_dir));
            said = true;
        }
        (waiting.nap)(stop);
        if stop.stopped() {
            return Ok(Waited::Stopped);
        }
    }
}
