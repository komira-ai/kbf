//! The host executor: running a placed action on a host leased from the job scheduler's
//! lease server instead of on a `kbf-daemon`.
//!
//! # How it sits in the server
//!
//! Neither scheduler core knows. A held lease becomes a virtual worker, `js:<host>`, that
//! registers with the [`Farm`] as a node whose only driver is `container`, and takes the
//! messages the farm sends a daemon on the channel it owns the receiving end of. A `Start` is
//! posted to the lease server as a run of the task tool in the action's own image; the host's
//! heartbeats trail the run, its outcome is mapped to the farm's failure classes
//! ([`outcome`]), and the result is reported the way a daemon reports one. `worker.rs`, the
//! two cores and the sched crates are untouched, and a server that never builds an
//! [`Executor`] never opens a connection to a lease server: [`ExecutorKind::KbfDaemon`] is the
//! path every server runs today.
//!
//! The executor runs on exactly one frontend: it holds its leases under its frontend id, and
//! on start stops every run of the leases it finds under that id, which nothing of the farm
//! maps to any more.
//!
//! Nothing wires it yet: there is no flag, and no server builds one.

mod config;
mod host;
mod outcome;
mod pool;
mod runs;
mod task;
mod traits;

use std::sync::Arc;
use std::time::Duration;

use futures::future::join_all;
use kbf_lease_client::{Client, Error, ErrorCode, Host, Lease, LeaseId};
use kbf_types::WorkerId;
use tokio::time::Instant;

use crate::Farm;
use pool::Slot;

pub use config::{Config, ConfigError};
pub use task::{Delivery, Nonce, PackKey, Secret, TaskArgs};
pub use traits::{
    ActionFacts, Ingest, Ingested, Inputs, InputsError, Presign, PresignError, ReadGrant,
    RejectReason, UntrustedLimits,
};

/// Which executor a server runs placed actions on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExecutorKind {
    /// The `kbf-daemon` of each node, over `kbf.worker.v1`: today's path, and the rollback.
    #[default]
    KbfDaemon,
    /// Hosts leased from the lease server, run by [`Executor`].
    JsAgent,
}

/// The server's wall clock at the moment the executor starts, and the monotonic clock from
/// there. All of the executor's timing is the monotonic clock; the wall clock only translates
/// the lease server's times.
#[derive(Clone, Copy, Debug)]
pub struct Clock {
    at_start: Instant,
    unix_ms: u64,
}

impl Clock {
    /// A clock that reads `unix_ms` (milliseconds since the Unix epoch) now. The monotonic
    /// part is tokio's, so a test that pauses it moves this clock with `advance`.
    #[must_use]
    pub fn from_unix_ms(unix_ms: u64) -> Self {
        Self {
            at_start: Instant::now(),
            unix_ms,
        }
    }

    fn now(&self) -> Instant {
        Instant::now()
    }

    /// The local time of the lease server's `unix_ms`; `now` for one in the past.
    fn local(&self, unix_ms: u64, now: Instant) -> Instant {
        let here = self.unix_ms + now.saturating_duration_since(self.at_start).as_millis() as u64;
        now + Duration::from_millis(unix_ms.saturating_sub(here))
    }
}

/// What the executor is built over.
#[derive(Clone)]
pub struct Deps {
    /// The farm the workers register with.
    pub farm: Arc<Farm>,
    /// The lease server.
    pub client: Client,
    /// The frontend's read of a task's pack.
    pub ingest: Arc<dyn Ingest>,
    /// The store's presigned `PUT`.
    pub presign: Arc<dyn Presign>,
    /// The CAS read of an action, and the read grant.
    pub inputs: Arc<dyn Inputs>,
}

/// What one pass reads once for every worker.
pub(crate) struct Ctx<'a> {
    pub(crate) cfg: &'a Config,
    pub(crate) deps: &'a Deps,
    pub(crate) clock: &'a Clock,
}

/// What the lease server said this pass.
#[derive(Default)]
pub(crate) struct View {
    leases: Vec<Lease>,
    hosts: Vec<Host>,
}

impl View {
    pub(crate) fn lease(&self, id: LeaseId) -> Option<&Lease> {
        self.leases.iter().find(|lease| lease.id == id)
    }

    pub(crate) fn host(&self, id: &str) -> Option<&Host> {
        self.hosts.iter().find(|host| host.id == id)
    }
}

/// Whether the lease server says the lease is no longer in force.
pub(crate) fn lease_gone(error: &Error) -> bool {
    [ErrorCode::UnknownLease, ErrorCode::NotHeld, ErrorCode::Expired]
        .iter()
        .any(|code| error.is(code))
}

/// A held host, as an operator or a test looks at it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostInfo {
    /// The worker the farm knows the host as.
    pub worker: WorkerId,
    /// The lease.
    pub lease: LeaseId,
    /// The host.
    pub host: String,
    /// The farm leases it holds: running, ingesting or waiting for an acknowledgement.
    pub runs: usize,
    /// Whether it has stopped taking work.
    pub draining: bool,
}

/// The pool of hosts, and what runs on them.
pub struct Executor {
    cfg: Config,
    deps: Deps,
    clock: Clock,
    slots: Vec<Slot>,
    adopted: bool,
    /// When the host list was last read.
    hosts_read: Option<Instant>,
}

impl Executor {
    /// An executor of `cfg` over `deps`. Nothing is asked of the lease server until the
    /// first [`step`](Self::step).
    ///
    /// # Errors
    /// `cfg` breaks a rule of [`Config::validate`].
    pub fn new(cfg: Config, deps: Deps, clock: Clock) -> Result<Self, ConfigError> {
        cfg.validate()?;
        Ok(Self {
            cfg,
            deps,
            clock,
            slots: Vec::new(),
            adopted: false,
            hosts_read: None,
        })
    }

    /// The hosts held now.
    #[must_use]
    pub fn hosts(&self) -> Vec<HostInfo> {
        self.slots.iter().filter_map(Slot::info).collect()
    }

    /// One pass over every slot: leases are asked for, kept and released, the farm's
    /// messages taken, runs posted and read, results reported, heartbeats sent. A caller
    /// steps it every [`Config::poll`]; a pass never waits on a run.
    pub async fn step(&mut self) {
        let now = self.clock.now();
        let Self {
            cfg,
            deps,
            clock,
            slots,
            adopted,
            hosts_read,
        } = self;
        let ctx = Ctx { cfg, deps, clock };
        if !*adopted {
            *slots = pool::adopt(&ctx, now).await.unwrap_or_default();
            *adopted = !slots.is_empty();
        }
        let view = read(&ctx, slots, hosts_read, now).await;
        join_all(slots.iter_mut().map(|slot| slot.drive(&ctx, &view, now))).await;
    }

    /// Stops what runs and releases every lease. The executor holds nothing afterwards, and
    /// asks for nothing more unless it is stepped.
    pub async fn shutdown(&mut self) {
        let ctx = Ctx {
            cfg: &self.cfg,
            deps: &self.deps,
            clock: &self.clock,
        };
        let slots = std::mem::take(&mut self.slots);
        join_all(slots.into_iter().map(|slot| slot.release(&ctx))).await;
        self.adopted = false;
    }
}

/// Reads what the slots need of the lease server: the booked leases while any slot waits for
/// one, and the host list every [`Config::host_check`] while any slot holds one.
async fn read(
    ctx: &Ctx<'_>,
    slots: &[Slot],
    hosts_read: &mut Option<Instant>,
    now: Instant,
) -> View {
    let waiting = slots.iter().any(Slot::is_pending);
    let leases = match waiting {
        true => ctx.deps.client.leases().await.unwrap_or_default(),
        false => Vec::new(),
    };
    let due = hosts_read.is_none_or(|at| now >= at + ctx.cfg.host_check);
    let holding = slots.iter().any(Slot::is_active);
    let hosts = match (due, holding) {
        (true, true) => {
            *hosts_read = Some(now);
            ctx.deps.client.hosts().await.unwrap_or_default()
        }
        _ => Vec::new(),
    };
    View { leases, hosts }
}
