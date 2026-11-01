//! The host executor: running a placed action on a host leased from the job scheduler's
//! lease server instead of on a `kbf-daemon`.
//!
//! # How it sits in the server
//!
//! Neither scheduler core knows. A held lease becomes a virtual worker, `js:<host>`, that
//! registers with the [`Farm`] as a node whose only driver is `container`, and takes the
//! messages the farm sends a daemon on the channel it owns the receiving end of. A `Start` is
//! posted to the lease server as a run of the kbf image, whose entrypoint is the task tool; the host's
//! heartbeats trail the run, its outcome is mapped to the farm's failure classes
//! ([`outcome`]), and the result is reported the way a daemon reports one. `worker.rs`, the
//! two cores and the sched crates are untouched, and a server that never builds an
//! [`Executor`] never opens a connection to a lease server: [`ExecutorKind::KbfDaemon`] is the
//! path every server runs today.
//!
//! The executor runs on exactly one frontend: it holds its leases under its frontend id and a
//! label of the process that made them, and on start stops every run of the leases an earlier
//! process left under that id, which nothing of the farm maps to any more. Those are taken only
//! once they are seen not to be renewed over [`Config::adopt_watch`]: a lease that is renewed
//! belongs to a live process under the same id (a copied config), and the executor stops
//! instead of killing its runs. [`Executor::rival`] finds another frontend's leases on the lease
//! server, and the task that steps the executor ([`Running`]) stops when it does. The id must be
//! different for each frontend.
//!
//! The frontend builds one only for `--executor js-agent` (`frontend/executor_flags.rs`), over
//! [`Front`], which implements the three seams ([`Ingest`], [`Presign`], [`Inputs`]) with the
//! frontend's own packs, CAS, key mint and run preparation, and steps it from
//! [`FrontendHandle::serve`](crate::frontend::FrontendHandle::serve) until the frontend stops.

mod api;
mod config;
mod driver;
mod front;
mod host;
mod load;
mod metrics;
mod outcome;
mod phases;
mod pool;
mod runs;
mod task;
mod traits;

#[cfg(test)]
#[path = "support_tests.rs"]
mod test_support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures::future::join_all;
use kbf_lease_client::{Client, Error, ErrorCode, Host, Lease, LeaseId, LeaseState};
use kbf_types::WorkerId;
use tokio::time::Instant;

use crate::Farm;
use pool::{Adopted, Adoption, Slot};

pub use config::{Config, ConfigError, DEFAULT_OUTPUT_DISK, DISK_MARGIN};
pub use driver::Running;
pub use front::{Front, FrontParts, PrepareError, PreparedRun, RunPrepare, RunSource};
pub use metrics::Metrics;
pub use task::{Delivery, Nonce, PackKey, Secret, TaskArgs, build_id};
pub use traits::{
    ActionFacts, Ingest, Ingested, Inputs, InputsError, MintError, Presign, PresignError,
    ReadGrant, RejectReason, UntrustedLimits,
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
///
/// The translation assumes the frontend's wall clock and the lease server's agree. If the lease
/// server's is ahead by `S`, the time a lease has left reads `S` too long, a run the lease
/// cuts short is not seen as cut, and its timeout is answered to the caller as the action's
/// own instead of being rerun. Hosts of one cluster keep their clocks within a few seconds,
/// which is what the margin of a run's wall time is for.
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
    /// What the executor counts.
    pub metrics: Arc<Metrics>,
}

/// What one pass reads once for every worker.
pub(crate) struct Ctx<'a> {
    pub(crate) cfg: &'a Config,
    pub(crate) deps: &'a Deps,
    pub(crate) clock: &'a Clock,
    pub(crate) strays: &'a Strays,
    pub(crate) bench: &'a Bench,
}

impl Ctx<'_> {
    /// The lease server, each call limited to [`Config::call_timeout`].
    pub(crate) fn api(&self) -> api::Api<'_> {
        api::Api {
            client: &self.deps.client,
            limit: self.cfg.call_timeout,
        }
    }
}

/// Whether a lease may exist that no slot holds: a request that may have booked one got no
/// answer, or a release failed. The next pass looks for it by label.
#[derive(Default)]
pub(crate) struct Strays(AtomicBool);

impl Strays {
    pub(crate) fn suspect(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    fn take(&self) -> bool {
        self.0.swap(false, Ordering::Relaxed)
    }
}

/// Leases of hosts that are held out of use: each host's scratch quota is out of order, and
/// its lease is kept, idle, until its time. The lease server is not asked for a named host, so
/// the only way to get a different one is to keep this one booked; at the time the lease is
/// released and the host may be leased again.
#[derive(Default)]
pub(crate) struct Bench(Mutex<Vec<(LeaseId, Instant)>>);

impl Bench {
    pub(crate) fn put(&self, lease: LeaseId, until: Instant) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((lease, until));
    }

    /// The leases held out, whatever their time.
    pub(crate) fn leases(&self) -> Vec<LeaseId> {
        let held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        held.iter().map(|(lease, _)| *lease).collect()
    }

    /// Takes the leases whose time has come.
    fn take_due(&self, now: Instant) -> Vec<LeaseId> {
        let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let (due, kept) = held.drain(..).partition(|(_, until)| now >= *until);
        *held = kept;
        due.into_iter().map(|(lease, _)| lease).collect()
    }

    /// Takes every lease, due or not.
    fn take_all(&self) -> Vec<LeaseId> {
        let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        held.drain(..).map(|(lease, _)| lease).collect()
    }
}

/// Releases `lease`. A release that did not reach the lease server leaves a lease nobody
/// holds, so the next pass looks for it; one the server answered, even with a refusal, leaves
/// nothing to look for.
pub(crate) async fn release_lease(ctx: &Ctx<'_>, lease: LeaseId) {
    let released = ctx.api().release(lease).await;
    note("releasing the lease", &released);
    if released.as_ref().err().is_some_and(|e| e.api().is_none()) {
        ctx.strays.suspect();
    }
}

/// What the lease server said this pass.
#[derive(Default)]
pub(crate) struct View {
    /// The booked leases; `None` if they were not read, or the read failed.
    leases: Option<Vec<Lease>>,
    hosts: Vec<Host>,
}

/// What a read of the lease server's leases says of one lease.
pub(crate) enum Listing<'a> {
    /// The list was not read, or could not be: nothing is known of the lease.
    Unread,
    /// The list was read and the lease is not in it.
    Absent,
    /// The lease, as listed.
    Found(&'a Lease),
}

impl View {
    pub(crate) fn lease(&self, id: LeaseId) -> Listing<'_> {
        match &self.leases {
            None => Listing::Unread,
            Some(leases) => leases
                .iter()
                .find(|lease| lease.id == id)
                .map_or(Listing::Absent, Listing::Found),
        }
    }

    pub(crate) fn host(&self, id: &str) -> Option<&Host> {
        self.hosts.iter().find(|host| host.id == id)
    }
}

/// Whether the lease server says the lease is no longer in force.
pub(crate) fn lease_gone(error: &Error) -> bool {
    [
        ErrorCode::UnknownLease,
        ErrorCode::NotHeld,
        ErrorCode::Expired,
    ]
    .iter()
    .any(|code| error.is(code))
}

/// Logs, once and in one place, that a call to the lease server failed and the caller goes on.
pub(crate) fn note<T>(what: &str, answer: &Result<T, Error>) {
    if let Err(error) = answer {
        tracing::warn!(%error, "{what} failed");
    }
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
    /// Whether the leases an earlier process of this frontend left have been taken up.
    adoption: Adoption,
    /// The leases taken up from an earlier process: this frontend releases them like its own.
    inherited: Vec<LeaseId>,
    /// Why the executor stopped itself: another process runs under this frontend's id.
    twin: Option<String>,
    strays: Strays,
    bench: Bench,
    /// When the leases were last compared with the slots'.
    reconciled: Option<Instant>,
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
            adoption: Adoption::Unread,
            inherited: Vec::new(),
            twin: None,
            strays: Strays::default(),
            bench: Bench::default(),
            reconciled: None,
            hosts_read: None,
        })
    }

    /// The hosts held now.
    #[must_use]
    pub fn hosts(&self) -> Vec<HostInfo> {
        self.slots.iter().filter_map(Slot::info).collect()
    }

    /// How often the caller steps the executor.
    #[must_use]
    pub fn poll(&self) -> Duration {
        self.cfg.poll
    }

    /// How often the caller checks that no other frontend holds hosts.
    #[must_use]
    pub fn rival_check(&self) -> Duration {
        self.cfg.reconcile_every
    }

    /// Why this executor must stop, if another frontend (or another process under this
    /// frontend's id) holds hosts on the lease server and is older: the executor runs on
    /// exactly one frontend, and two would each stop the other's runs on restart and share
    /// hosts the farm knows one of. Of two that hold hosts the one whose oldest lease is
    /// younger stops (ties go to the lower frontend id, then instance), so two that started
    /// together and both leased are not both stopped, and a frontend that finds hosts held
    /// before it holds any stops at once. Nothing is concluded from a list that could not be
    /// read.
    ///
    /// Leases of another process under this frontend's own id are not judged here until the
    /// leases of an earlier process have been taken up: that is [`Adoption`]'s question.
    pub async fn rival(&self) -> Option<String> {
        let ctx = Ctx {
            cfg: &self.cfg,
            deps: &self.deps,
            clock: &self.clock,
            strays: &self.strays,
            bench: &self.bench,
        };
        let leases = ctx.api().leases().await.ok()?;
        let held: Vec<LeaseId> = self.slots.iter().filter_map(Slot::lease).collect();
        let me = (self.cfg.frontend.as_str(), self.cfg.instance.as_str());
        let mut mine = u64::MAX;
        let mut others = Vec::new();
        for lease in leases {
            let live = matches!(lease.state, LeaseState::Held | LeaseState::Pending);
            let Some(frontend) = lease.labels.get(pool::FRONTEND_LABEL).filter(|_| live) else {
                continue;
            };
            let instance = lease
                .labels
                .get(pool::INSTANCE_LABEL)
                .map_or("", String::as_str);
            if held.contains(&lease.id) || (frontend.as_str(), instance) == me {
                mine = mine.min(lease.acquired_at_ms);
            } else if *frontend != self.cfg.frontend || self.adoption.is_done() {
                others.push((lease.acquired_at_ms, frontend.clone(), instance.to_owned()));
            }
        }
        let older = others
            .into_iter()
            .filter(|(since, frontend, instance)| {
                (*since, frontend.as_str(), instance.as_str()) < (mine, me.0, me.1)
            })
            .min()?;
        Some(match older.1 == self.cfg.frontend {
            true => format!(
                "another frontend holds hosts under the same id {:?}: --executor-frontend-id must be different for each frontend",
                older.1
            ),
            false => format!(
                "the frontend {:?} holds hosts on the lease server too: the executor runs on exactly one frontend",
                older.1
            ),
        })
    }

    /// Why the executor stopped itself without being asked, if it did: another process runs
    /// under this frontend's id ([`Executor::new`]'s `Config::frontend`).
    #[must_use]
    pub fn twin(&self) -> Option<&str> {
        self.twin.as_deref()
    }

    /// One pass over every slot: leases are asked for, kept and released, the farm's
    /// messages taken, runs posted and read, results reported, heartbeats sent. A caller
    /// steps it every [`Config::poll`]; a pass never waits on a run. A pass is run to its end:
    /// dropping it half way leaves a slot that held a lease vacant, and the lease unreleased
    /// (it expires, and a restarted frontend finds it by its label).
    pub async fn step(&mut self) {
        let now = self.clock.now();
        let Self {
            cfg,
            deps,
            clock,
            slots,
            adoption,
            inherited,
            twin,
            strays,
            bench,
            reconciled,
            hosts_read,
        } = self;
        if twin.is_some() {
            return;
        }
        let ctx = Ctx {
            cfg,
            deps,
            clock,
            strays,
            bench,
        };
        if !adoption.is_done() {
            match pool::adopt(&ctx, now, adoption).await {
                Adopted::Slots { held, left } => {
                    *slots = held;
                    *inherited = left;
                    *adoption = Adoption::Done;
                }
                // The leases of an earlier process are not yet known to be nobody's: nothing is
                // asked for, so no host is taken while a live twin might hold them.
                Adopted::Watching => return,
                Adopted::Twin(why) => {
                    *twin = Some(why);
                    return;
                }
                Adopted::Unread => {}
            }
            *reconciled = Some(now);
        }
        let due = reconciled.is_none_or(|at| now >= at + cfg.reconcile_every);
        if strays.take() || due {
            *reconciled = Some(now);
            if !pool::reconcile(&ctx, slots, inherited).await {
                strays.suspect();
            }
        }
        // Before the slots are driven, so a host whose hold has ended is free to lease again
        // in this pass.
        for lease in bench.take_due(now) {
            release_lease(&ctx, lease).await;
        }
        let view = read(&ctx, slots, hosts_read, now).await;
        join_all(slots.iter_mut().map(|slot| slot.drive(&ctx, &view, now))).await;
        deps.metrics
            .hold(slots.iter().filter(|slot| slot.is_active()).count());
    }

    /// Stops what runs and releases every lease. The executor holds nothing afterwards, and
    /// asks for nothing more unless it is stepped.
    pub async fn shutdown(&mut self) {
        let ctx = Ctx {
            cfg: &self.cfg,
            deps: &self.deps,
            clock: &self.clock,
            strays: &self.strays,
            bench: &self.bench,
        };
        let slots = std::mem::take(&mut self.slots);
        join_all(slots.into_iter().map(|slot| slot.release(&ctx))).await;
        for lease in self.bench.take_all() {
            release_lease(&ctx, lease).await;
        }
        self.adoption = Adoption::Unread;
        self.inherited.clear();
        self.twin = None;
        self.deps.metrics.hold(0);
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
        true => {
            let listed = ctx.api().leases().await;
            note("listing the leases", &listed);
            listed.ok()
        }
        false => None,
    };
    let due = hosts_read.is_none_or(|at| now >= at + ctx.cfg.host_check);
    let holding = slots.iter().any(Slot::is_active);
    let hosts = match (due, holding) {
        (true, true) => {
            *hosts_read = Some(now);
            ctx.api().hosts().await.unwrap_or_default()
        }
        _ => Vec::new(),
    };
    View { leases, hosts }
}
