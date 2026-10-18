//! The operator's verbs: `confirm`, `revert`, `rollback`, `gc` and `quiesce`. Each runs
//! inline, as root, in the operator's process, under the same lock as the service's
//! passes, so a pass and a verb cannot both change the state. Without `apply` a verb
//! makes every check and changes nothing.
//!
//! A verb that moves a link owes the service a restart. It leaves a marker
//! ([`restart::owed_marker`]) before the state changes, restarts the service itself while
//! it still holds the lock, and removes the marker; a verb killed in between leaves the
//! marker, and the next pass of `kbf-installer run` does the restart.

use std::io::Write;
use std::time::Duration;

use kbf_host::plan::Mode;
use kbf_host::{Exec, Perms, Step};

use crate::activate::{self, Reverted};
use crate::helper::restart::{self, Restart};
use crate::helper::service::{self, Procs};
use crate::layout::Layout;
use crate::release::{self, ReleaseError};
use crate::state::{Locked, State, StateError};

/// How long a verb waits for a pass (or another verb) to let go of the lock.
pub const LOCK_WAIT: Duration = Duration::from_secs(10);

/// Why a verb did not do what it was asked; each is an exit code of the CLI.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Failure {
    /// Another run holds the lock (exit 4).
    #[error("busy: {0}")]
    Busy(String),
    /// The checks refused it (exit 3).
    #[error("refused: {0}")]
    Refused(String),
    /// There is nothing for the verb to act on, or only the operator can say what to do
    /// (exit 5).
    #[error("{0}")]
    NeedsOperator(String),
    /// It could not be done (exit 1).
    #[error("{0}")]
    Failed(String),
}

impl Failure {
    pub fn exit_code(&self) -> u8 {
        match self {
            Failure::Failed(_) => 1,
            Failure::Refused(_) => 3,
            Failure::Busy(_) => 4,
            Failure::NeedsOperator(_) => 5,
        }
    }
}

impl From<StateError> for Failure {
    fn from(e: StateError) -> Failure {
        match e {
            StateError::Busy { .. } => Failure::Busy(e.to_string()),
            other => Failure::Failed(other.to_string()),
        }
    }
}

impl From<ReleaseError> for Failure {
    fn from(e: ReleaseError) -> Failure {
        match e {
            ReleaseError::State(s) => s.into(),
            ReleaseError::NotPending(_) | ReleaseError::NoPrevious | ReleaseError::Pending(_) => {
                Failure::NeedsOperator(e.to_string())
            }
            ReleaseError::OtherOrder { .. } => Failure::Refused(e.to_string()),
            other => Failure::Failed(other.to_string()),
        }
    }
}

/// A host, the service on it, and whether the verbs change anything.
pub struct Operator<'a, E: Exec> {
    pub host: &'a Layout,
    pub exec: &'a mut E,
    pub restart: &'a Restart,
    pub procs: &'a dyn Procs,
    pub out: &'a mut dyn Write,
    pub apply: bool,
    /// How long to wait for the lock ([`LOCK_WAIT`]).
    pub wait: Duration,
}

impl<E: Exec> Operator<'_, E> {
    fn say(&mut self, line: &str) {
        let _ = writeln!(self.out, "{line}");
    }

    fn locked(&self) -> Result<(Locked, State), Failure> {
        let lock = Locked::acquire_within(self.host, self.wait)?;
        let state = lock.read()?.unwrap_or_default();
        Ok((lock, state))
    }

    /// The swap on trial, if it is the one `activation` names.
    fn pending(state: &State, activation: &str) -> Result<crate::state::PendingConfirm, Failure> {
        let Some(p) = &state.pending_confirm else {
            return Err(Failure::NeedsOperator("no swap is pending".to_owned()));
        };
        if p.order_id.as_deref() != Some(activation) {
            return Err(Failure::Refused(format!(
                "the pending swap of {} was made by {}, not by {activation:?}",
                p.commit,
                p.order_id.as_deref().unwrap_or("(none)")
            )));
        }
        Ok(p.clone())
    }

    /// Keep the pending swap `activation`, if the new release is what runs.
    pub fn confirm(&mut self, activation: &str) -> Result<(), Failure> {
        let (lock, state) = self.locked()?;
        let p = Self::pending(&state, activation)?;
        service::accept_confirm(&*self.exec, self.procs, self.host, self.restart, &p)
            .map_err(Failure::Refused)?;
        if !self.apply {
            self.say(&format!("would confirm {} ({activation})", p.commit));
            return Ok(());
        }
        activate::confirm_in(&lock, state, activation, &p.commit)?;
        self.say(&format!("confirmed {} ({activation})", p.commit));
        Ok(())
    }

    /// Put `current` back on the release that ran before the pending swap `activation`.
    pub fn revert(&mut self, activation: &str) -> Result<(), Failure> {
        let (lock, state) = self.locked()?;
        let p = Self::pending(&state, activation)?;
        if !self.apply {
            self.say(&format!("would revert {} ({activation})", p.commit));
            return Ok(());
        }
        let done = self.moving(&lock, &state, |exec, out| {
            activate::revert_in(exec, &lock, state.clone(), out)
        })?;
        self.say(&format!(
            "reverted {} ({activation}): {} is current",
            p.commit, done.to
        ));
        Ok(())
    }

    /// `current` back to `previous`, with no window; refused while a swap is pending.
    pub fn rollback(&mut self) -> Result<(), Failure> {
        let (lock, state) = self.locked()?;
        let to = activate::rollback_target(&*self.exec, &state)?;
        if !self.apply {
            self.say(&format!("would roll back to {to}"));
            return Ok(());
        }
        let done = self.moving(&lock, &state, |exec, out| {
            activate::rollback_in(exec, &lock, state.clone(), out)
        })?;
        self.say(&format!("rolled back: {} is current", done.to));
        Ok(())
    }

    /// Run `change` (which moves the links) with the restart it owes marked first, then
    /// restart the service, still under the lock.
    fn moving(
        &mut self,
        lock: &Locked,
        before: &State,
        change: impl FnOnce(&mut E, &mut dyn Write) -> Result<Reverted, ReleaseError>,
    ) -> Result<Reverted, Failure> {
        let marker = restart::owed_marker(self.host);
        self.exec
            .apply(&Step::WriteFile {
                path: marker.clone(),
                perms: Perms::new(0o600),
                owner: None,
                bytes: b"owed\n".to_vec(),
            })
            .map_err(|e| Failure::Failed(format!("cannot write the restart marker: {e}")))?;
        let result = change(&mut *self.exec, &mut *self.out);
        let moved = lock.read().ok().flatten().map(|s| s.current) != Some(before.current.clone());
        let done = match result {
            Ok(done) => done,
            Err(e) => {
                // Nothing moved: the marker would only cause a restart for nothing.
                if !moved {
                    let _ = self.exec.apply(&Step::Remove { path: marker });
                }
                return Err(e.into());
            }
        };
        self.exec.apply(&self.restart.step()).map_err(|e| {
            Failure::Failed(format!(
                "the restart failed; the service's next pass retries it: {e}"
            ))
        })?;
        self.exec
            .apply(&Step::Remove { path: marker })
            .map_err(|e| Failure::Failed(format!("cannot remove the restart marker: {e}")))?;
        Ok(done)
    }
}

/// `gc`: remove the releases nothing needs. Keeps current, previous, staged, the pending
/// swap's, every release a process executes from, and the `keep` newest others.
pub fn gc<E: Exec>(
    exec: &mut E,
    host: &Layout,
    procs: &dyn Procs,
    keep: usize,
    apply: bool,
    out: &mut dyn Write,
) -> Result<(), Failure> {
    let in_use = service::in_use(procs, host)
        .map_err(|e| Failure::Failed(format!("cannot tell which releases run: {e}")))?;
    let mode = if apply { Mode::Apply } else { Mode::DryRun };
    release::gc(exec, host, keep, &in_use, mode, out)?;
    Ok(())
}

/// `quiesce`, for a maintainer script before it replaces the package's files: wait out any
/// pass or verb that is changing the state, and say whether the host is at rest (nothing
/// on trial). The service starts the new program itself between passes.
pub fn quiesce(host: &Layout, wait: Duration) -> Result<(), Failure> {
    let lock = Locked::acquire_within(host, wait)?;
    match lock.read()?.and_then(|s| s.pending_confirm) {
        Some(p) => Err(Failure::NeedsOperator(format!(
            "{} is on trial and not yet confirmed or reverted",
            p.commit
        ))),
        None => Ok(()),
    }
}

#[cfg(test)]
#[path = "operator_tests.rs"]
mod tests;
