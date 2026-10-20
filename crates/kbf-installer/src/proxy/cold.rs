//! Starting a release where no child serves, and making it take the generation (FP
//! section 6, "both children dead"; section 8, "a cold start bumps the epoch").
//!
//! One machinery, two callers: [`Swap::cold_start`](super::swap::Swap) after a restart in
//! the middle of an update, and the supervisor's boot ([`super::supervisor`]) on a host
//! with no live child. The steps are the same in both: spawn the release as a candidate in
//! its slot (the child port records the process before it returns), wait until the process
//! is ready, make it take the generation (with no live holder the child bumps the epoch),
//! then release the gate into the slot. This module has no update record: the swap saves its
//! own intent between [`Cold::spawn`] and [`Cold::await_ready`].

use std::time::Duration;

use super::model::{Halt, PortError, ProcIdent, RoleInfo, Slot, Start, Timing};
use super::ports::{Child, ChildAdmin, Clock, Gate};
use crate::CommitId;

/// Why a step did not complete.
#[derive(Debug, thiserror::Error)]
pub(super) enum ColdError {
    #[error("spawn: {0}")]
    Spawn(PortError),
    #[error("the new process exited with {0}")]
    Exited(i32),
    #[error("not ready in {0} s")]
    NotReady(u64),
    #[error("takeover: {0}")]
    Takeover(PortError),
    /// The takeover landed and the gate refused to route to the slot.
    #[error("release into {slot:?}: {error}")]
    Release { slot: Slot, error: PortError },
    /// The proxy is shutting down.
    #[error("stopped: {0:?}")]
    Stopped(Halt),
}

/// What "ready" asks of a candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReadyRule {
    /// All three of [`Readiness`](super::model::Readiness): `/readyz`, a health check and a
    /// synthetic `GetCapabilities`. The swap's rule (FP section 5 step 5).
    Full,
    /// `/readyz` of the operator port alone. The proxy's boot uses this one: the health and
    /// capabilities probes are not built, so [`Admin`](super::admin::Admin) reports them
    /// `false` for every child and [`ReadyRule::Full`] can never pass in a running proxy. The
    /// takeover that follows is the final check: a candidate that cannot serve refuses it.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Readyz,
}

/// The ports a cold start drives, and its limits.
#[derive(Clone, Copy)]
pub(super) struct Cold<'a> {
    pub child: &'a dyn Child,
    pub admin: &'a dyn ChildAdmin,
    pub gate: &'a dyn Gate,
    pub clock: &'a dyn Clock,
    pub timing: Timing,
    pub ready: ReadyRule,
}

impl Cold<'_> {
    /// Start `release` in `slot` as a candidate. The identity is on disk when this returns.
    pub fn spawn(&self, slot: Slot, release: &CommitId) -> Result<ProcIdent, ColdError> {
        self.child
            .spawn(slot, release, Start::Candidate)
            .map_err(ColdError::Spawn)
    }

    /// Wait until `ident` in `slot` answers ready.
    pub fn await_ready(&self, slot: Slot, ident: &ProcIdent) -> Result<(), ColdError> {
        self.wait(ident, || {
            matches!(self.admin.ready(slot), Ok(r) if match self.ready {
                ReadyRule::Full => r.all(),
                ReadyRule::Readyz => r.readyz,
            })
            .then_some(())
        })
    }

    /// Wait until `ident` in `slot` answers `role`.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn await_role(&self, slot: Slot, ident: &ProcIdent) -> Result<RoleInfo, ColdError> {
        self.wait(ident, || self.admin.role(slot).ok())
    }

    /// Make `slot`, the only live child, take the generation and route to it. Returns how
    /// many held streams expired just before the release.
    pub fn take_over(&self, slot: Slot) -> Result<usize, ColdError> {
        self.admin
            .takeover(slot, self.timing.takeover)
            .map_err(ColdError::Takeover)?;
        let expired = self.gate.expire();
        self.gate
            .release_into(slot)
            .map_err(|error| ColdError::Release { slot, error })?;
        Ok(expired)
    }

    /// Poll `probe` every `poll` until it answers, the process exits, or `ready_within`
    /// passes.
    fn wait<T>(
        &self,
        ident: &ProcIdent,
        mut probe: impl FnMut() -> Option<T>,
    ) -> Result<T, ColdError> {
        let first: Duration = self.clock.now();
        loop {
            if let Some(code) = self.child.exited(ident) {
                return Err(ColdError::Exited(code));
            }
            if let Some(answer) = probe() {
                return Ok(answer);
            }
            if self.clock.now().saturating_sub(first) >= self.timing.ready_within {
                return Err(ColdError::NotReady(self.timing.ready_within.as_secs()));
            }
            self.clock
                .sleep(self.timing.poll)
                .map_err(ColdError::Stopped)?;
        }
    }
}
