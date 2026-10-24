//! The coordinator: the fleet the heartbeats build, the desired state, and who may change
//! or read either. No sockets here; [`crate::http`] maps requests onto these calls.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_oidc::{Refusal, Verifier};
use kbf_updater::wire::{Heartbeat, Reply};
use serde::Serialize;
use subtle::ConstantTimeEq;

use crate::decide::decide;
use crate::desired::{Desired, DesiredError, DesiredStore, StoreError};
use crate::fleet::{Fleet, QUIET_START};
use crate::identity::Identity;

/// The time since the coordinator started.
pub type Clock = Arc<dyn Fn() -> Duration + Send + Sync>;

/// The most bytes of a bearer token looked at.
pub const MAX_BEARER_BYTES: usize = 8192;

/// Why the desired state was not set.
#[derive(Debug, thiserror::Error)]
pub enum SetError {
    /// The caller has no valid CI token: no bearer, or the verifier refused it (the name is
    /// the verifier's short reason; the operator token is refused like any bad token).
    #[error("unauthorized: {0}")]
    Unauthorized(&'static str),
    /// The token's issuer keys could not be fetched.
    #[error("the issuer's keys are unavailable")]
    Unavailable,
    /// The body is no desired state.
    #[error(transparent)]
    Invalid(#[from] DesiredError),
    /// The state file could not be written.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// The caller of a read has no valid operator token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unauthorized")]
pub struct Unauthorized;

/// One host in the status.
#[derive(Debug, Serialize)]
pub struct HostStatus {
    /// The node id from its certificate.
    pub node_id: String,
    /// The host role from its certificate.
    pub role: &'static str,
    /// Heartbeated within the down limit.
    pub live: bool,
    /// Live, healthy and not updating.
    pub up: bool,
    /// Holds one of its role's update slots.
    pub holds_slot: bool,
    /// Whole seconds since its last heartbeat.
    pub silent_secs: u64,
    /// What it last said.
    #[serde(flatten)]
    pub beat: Heartbeat,
}

/// What an operator reads.
#[derive(Debug, Serialize)]
pub struct Status {
    /// The desired state, if one was ever set.
    pub desired: Option<Desired>,
    /// The coordinator is still within its quiet period and orders nothing.
    pub quiet: bool,
    /// Every host heard from since the coordinator started, by node id.
    pub hosts: Vec<HostStatus>,
}

/// The coordinator.
pub struct Coordinator {
    fleet: Mutex<Fleet>,
    desired: DesiredStore,
    verifier: Arc<Verifier>,
    operator_token: String,
    clock: Clock,
}

impl std::fmt::Debug for Coordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Coordinator").finish_non_exhaustive()
    }
}

impl Coordinator {
    /// A coordinator that starts now, by `clock`, with the desired state of `desired`. CI's
    /// token is checked by `verifier`; the operator's read is `operator_token`.
    #[must_use]
    pub fn new(
        desired: DesiredStore,
        verifier: Arc<Verifier>,
        operator_token: String,
        clock: Clock,
    ) -> Self {
        let started = clock();
        Self {
            fleet: Mutex::new(Fleet::new(started)),
            desired,
            verifier,
            operator_token,
            clock,
        }
    }

    /// Notes a heartbeat from `who` and answers it. With no desired state set, nothing.
    pub fn heartbeat(&self, who: Identity, beat: Heartbeat) -> Reply {
        let now = (self.clock)();
        let mut fleet = self.fleet.lock().unwrap_or_else(PoisonError::into_inner);
        fleet.record(who.node_id.clone(), who.role, beat, now);
        self.desired
            .get()
            .map_or(Reply::Nothing, |d| decide(&d, &fleet, &who.node_id, now))
    }

    /// Sets the desired state from `body`, if `bearer` is a GitHub Actions OIDC token the
    /// allow-list accepts. The check comes before the body is read.
    ///
    /// # Errors
    /// [`SetError`].
    pub async fn set_desired(
        &self,
        bearer: Option<&str>,
        body: &[u8],
    ) -> Result<Desired, SetError> {
        let token = bearer
            .filter(|t| t.len() <= MAX_BEARER_BYTES)
            .ok_or(SetError::Unauthorized("missing"))?;
        match self.verifier.verify(token).await {
            Ok(verified) => tracing::info!(
                workflow = %verified.job_workflow_ref,
                sha = ?verified.sha,
                "desired state set by CI"
            ),
            Err(Refusal::KeysUnavailable) => return Err(SetError::Unavailable),
            Err(refusal) => return Err(SetError::Unauthorized(refusal.name())),
        }
        let desired = Desired::parse(body)?;
        self.desired.set(desired.clone())?;
        Ok(desired)
    }

    /// The status, if `bearer` is the operator token.
    ///
    /// # Errors
    /// [`Unauthorized`].
    pub fn status(&self, bearer: Option<&str>) -> Result<Status, Unauthorized> {
        let presented = bearer.ok_or(Unauthorized)?.as_bytes();
        if !bool::from(presented.ct_eq(self.operator_token.as_bytes())) {
            return Err(Unauthorized);
        }
        let now = (self.clock)();
        let fleet = self.fleet.lock().unwrap_or_else(PoisonError::into_inner);
        let hosts = fleet
            .hosts()
            .map(|(id, host)| HostStatus {
                node_id: id.to_string(),
                role: host.role.name(),
                live: host.is_live(now),
                up: host.is_up(now),
                holds_slot: host.holds_slot(),
                silent_secs: now.saturating_sub(host.seen).as_secs(),
                beat: host.beat.clone(),
            })
            .collect();
        Ok(Status {
            desired: self.desired.get(),
            quiet: now < fleet.started().saturating_add(QUIET_START),
            hosts,
        })
    }
}
