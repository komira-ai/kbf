//! The coordinator: the fleet the heartbeats build, the desired state, and who may change
//! or read either. No sockets here; [`crate::http`] maps requests onto these calls.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_oidc::{Refusal, Verified, Verifier};
use kbf_updater::wire::{Heartbeat, Reply, ServiceReply};
use serde::Serialize;
use subtle::ConstantTimeEq;

use crate::converge::{Convergence, RoleCounts, converge, converge_service, role_counts};
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
    /// The allow-list entry that admitted the token does not name this service.
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

/// One service in the status.
#[derive(Debug, Serialize)]
pub struct ServiceStatus {
    /// Its desired state, if one was ever set.
    pub desired: Option<Desired>,
    /// The counts of every role that has a host heartbeating the service, in role order.
    pub roles: Vec<RoleCounts>,
}

/// Why the convergence was not read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConvergenceError {
    /// The caller's CI token was not accepted.
    #[error(transparent)]
    Refused(#[from] CiRefused),
    /// The coordinator manages no such service.
    #[error("no such service")]
    UnknownService,
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
    /// Every host heard from since the coordinator started, by node id.
    pub hosts: Vec<HostStatus>,
    /// Every service the coordinator manages, `kbf` included, by name: its desired state and
    /// the counts of its hosts per role.
    pub per_service: BTreeMap<String, ServiceStatus>,
    /// The commit this coordinator was built from ([`crate::BUILD_COMMIT`]).
    pub commit: &'static str,
}

/// The coordinator.
pub struct Coordinator {
    fleet: Mutex<Fleet>,
    desired: DesiredStore,
    services: Services,
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
            services: Services::default(),
            verifier,
            operator_token,
            clock,
        }
    }

    /// The same coordinator over `services` instead of `kbf` alone.
    #[must_use]
    pub fn with_services(mut self, services: Services) -> Self {
        self.services = services;
        self
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
        let (service, desired) =
            Desired::parse_request_for(body, |name| self.services.get(name).is_some())?;
        if !verified.services.contains(&service) {
            tracing::warn!(
                workflow = %verified.job_workflow_ref,
                %service,
                "refused a desired state: the allow-list entry does not name the service"
            );
            return Err(SetError::Forbidden);
        }
        self.desired.set(&service, desired.clone())?;
        tracing::info!(
            workflow = %verified.job_workflow_ref,
            sha = ?verified.sha,
            %service,
            "desired state set by CI"
        );
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
        let per_service = self
            .services
            .names()
            .map(|name| {
                let desired = self.desired.get(name);
                let roles = role_counts(name, desired.as_ref(), &fleet, now);
                (name.to_owned(), ServiceStatus { desired, roles })
            })
            .collect();
        Ok(Status {
            per_service,
            desired: self.desired.get(KBF),
            services: self.desired.all(),
            quiet: fleet.is_quiet(now),
            hosts,
            commit: crate::BUILD_COMMIT,
        })
    }

    /// Where the hosts of `service` stand against its desired state, if `bearer` is a
    /// GitHub Actions OIDC token the same allow-list accepts as for [`Self::set_desired`]
    /// (any service may be read with any accepted token). The operator token is no CI token
    /// and is refused. Read-only, and each token is good once.
    ///
    /// # Errors
    /// [`ConvergenceError`].
    pub async fn convergence(
        &self,
        bearer: Option<&str>,
        service: &str,
    ) -> Result<Convergence, ConvergenceError> {
        self.ci(bearer).await?;
        let def = self
            .services
            .get(service)
            .ok_or(ConvergenceError::UnknownService)?;
        let now = (self.clock)();
        let fleet = self.fleet.lock().unwrap_or_else(PoisonError::into_inner);
        let desired = self.desired.get(service);
        Ok(if service == KBF {
            converge(desired.as_ref(), &fleet, now)
        } else {
            converge_service(def, desired.as_ref(), &fleet, now)
        })
    }
}
