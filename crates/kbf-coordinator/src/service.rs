//! The coordinator: the fleet the heartbeats build, the desired state, and who may change
//! or read either. No sockets here; [`crate::http`] maps requests onto these calls.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_oidc::{Refusal, Verified, Verifier};
use kbf_updater::wire::{Heartbeat, Reply, ServiceReply};
use serde::Serialize;
use subtle::ConstantTimeEq;

use crate::converge::{Convergence, converge};
use crate::decide::decide_in;
use crate::desired::{Desired, DesiredError, DesiredStore, KBF, StoreError};
use crate::fleet::Fleet;
use crate::identity::Identity;
use crate::services::Services;

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
    /// The token's allow-list entry may not set this service.
    #[error("the token may not set this service")]
    Forbidden,
}

/// Why a CI token was not accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CiRefused {
    /// No bearer, or the verifier refused it (the name is the verifier's short reason; the
    /// operator token is refused like any bad token).
    #[error("unauthorized: {0}")]
    Unauthorized(&'static str),
    /// The token's issuer keys could not be fetched.
    #[error("the issuer's keys are unavailable")]
    Unavailable,
}

impl From<CiRefused> for SetError {
    fn from(refused: CiRefused) -> Self {
        match refused {
            CiRefused::Unauthorized(why) => Self::Unauthorized(why),
            CiRefused::Unavailable => Self::Unavailable,
        }
    }
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
    /// The desired state of `kbf`, if one was ever set.
    pub desired: Option<Desired>,
    /// The desired state of every service, by name.
    pub services: BTreeMap<String, Desired>,
    /// The coordinator is still within its quiet period and orders nothing.
    pub quiet: bool,
    /// Heartbeats since the coordinator started whose `services` list was bad and was
    /// dropped: the host was answered as a `kbf`-only host.
    pub services_dropped: u64,
    /// Every host heard from since the coordinator started, by node id.
    pub hosts: Vec<HostStatus>,
}

/// The coordinator.
pub struct Coordinator {
    fleet: Mutex<Fleet>,
    desired: DesiredStore,
    services: Services,
    verifier: Arc<Verifier>,
    operator_token: String,
    clock: Clock,
    services_dropped: AtomicU64,
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
            services: Services::default(),
            verifier,
            operator_token,
            clock,
            services_dropped: AtomicU64::new(0),
        }
    }

    /// The same coordinator over `services` instead of `kbf` alone.
    #[must_use]
    pub fn with_services(mut self, services: Services) -> Self {
        self.services = services;
        self
    }

    /// Counts and logs a heartbeat from `who` whose `services` list was bad and was dropped
    /// (`why` is the reader's reason). The heartbeat itself is still answered, by
    /// [`Self::heartbeat`], as one from a `kbf`-only host.
    pub fn note_services_dropped(&self, who: &Identity, why: &str) {
        self.services_dropped.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(node_id = %who.node_id, why, "dropped the services list of a heartbeat");
    }

    /// Notes a heartbeat from `who` and answers it. With no desired state set, nothing.
    ///
    /// The top-level fields are `kbf`. Each entry of `services` is noted as that service's
    /// heartbeat of the host, if the coordinator manages the service and runs it on the
    /// host's role (any other entry is ignored). `kbf` is decided first, then the host's
    /// services in the order it listed them, and the first order is the reply: one update
    /// per reply. A host that sent no `services` is answered about `kbf` alone, in the
    /// shape it has always read.
    pub fn heartbeat(&self, who: Identity, beat: Heartbeat) -> ServiceReply {
        let now = (self.clock)();
        let mut fleet = self.fleet.lock().unwrap_or_else(PoisonError::into_inner);
        let announced = beat.services.clone().unwrap_or_default();
        let platform = beat.platform;
        fleet.record(who.node_id.clone(), who.role, beat, now);
        for one in &announced {
            let runs_here = self
                .services
                .get(&one.name)
                .is_some_and(|def| def.roles.contains_key(&who.role));
            if runs_here {
                fleet.record_in(
                    &one.name,
                    who.node_id.clone(),
                    who.role,
                    one.as_heartbeat(platform),
                    now,
                );
            }
        }
        let desired = self.desired.all();
        let names = std::iter::once(KBF).chain(announced.iter().map(|one| one.name.as_str()));
        for name in names {
            let reply = decide_in(&self.services, &desired, name, &fleet, &who.node_id, now);
            if let Reply::Update { version } = reply {
                fleet.order(name, who.node_id.clone(), version.clone(), now);
                return ServiceReply::Update {
                    service: (name != KBF).then(|| name.to_owned()),
                    version,
                };
            }
        }
        ServiceReply::Nothing
    }

    /// The CI token in `bearer`, checked by the verifier.
    async fn ci(&self, bearer: Option<&str>) -> Result<Verified, CiRefused> {
        let token = bearer
            .filter(|t| t.len() <= MAX_BEARER_BYTES)
            .ok_or(CiRefused::Unauthorized("missing"))?;
        match self.verifier.verify(token).await {
            Ok(verified) => Ok(verified),
            Err(Refusal::KeysUnavailable) => Err(CiRefused::Unavailable),
            Err(refusal) => Err(CiRefused::Unauthorized(refusal.name())),
        }
    }

    /// Sets the desired state from `body`, if `bearer` is a GitHub Actions OIDC token the
    /// allow-list accepts. The token is checked before the body is parsed or any state is touched
    /// (the transport has already read the body, at most [`crate::http::MAX_DESIRED_BYTES`]).
    ///
    /// # Errors
    /// [`SetError`].
    pub async fn set_desired(
        &self,
        bearer: Option<&str>,
        body: &[u8],
    ) -> Result<Desired, SetError> {
        let verified = self.ci(bearer).await?;
        tracing::info!(
            workflow = %verified.job_workflow_ref,
            sha = ?verified.sha,
            "desired state set by CI"
        );
        let (service, desired) =
            Desired::parse_request_for(body, |name| self.services.get(name).is_some())?;
        if !self.services.ci_may_set(&service) {
            return Err(SetError::Forbidden);
        }
        self.desired.set(&service, desired.clone())?;
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
                up: fleet.is_up(KBF, id, host, now),
                holds_slot: fleet.holds_slot(KBF, id, host, now),
                silent_secs: now.saturating_sub(host.seen).as_secs(),
                beat: host.beat.clone(),
            })
            .collect();
        Ok(Status {
            desired: self.desired.get(KBF),
            services: self.desired.all(),
            quiet: fleet.is_quiet(now),
            services_dropped: self.services_dropped.load(Ordering::Relaxed),
            hosts,
        })
    }

    /// Where the farm stands against the desired state, if `bearer` is a GitHub Actions
    /// OIDC token the same allow-list accepts as for [`Self::set_desired`]. The operator
    /// token is no CI token and is refused. Read-only, and each token is good once.
    ///
    /// # Errors
    /// [`CiRefused`].
    pub async fn convergence(&self, bearer: Option<&str>) -> Result<Convergence, CiRefused> {
        self.ci(bearer).await?;
        let now = (self.clock)();
        let fleet = self.fleet.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(converge(self.desired.get(KBF).as_ref(), &fleet, now))
    }
}
