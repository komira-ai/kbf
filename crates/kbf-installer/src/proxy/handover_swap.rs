//! The real supervisor of the update API as the proxy verb hosts it, and the slot that lets it
//! be built before the proxy is bound.
//!
//! [`super::handover::parts`] runs before `Proxy::bind` (an order spool the service user
//! cannot open is a usage error before anything is bound), and the swap's ports need what
//! only exists after it: the proxy's gate, the children's process control, the secret the
//! proxy sends the children, and what stops the process when the swap thread dies. The verb
//! learns those in `supervise` and puts them in a [`GateSlot`] that [`HandoverConfig`] carries
//! (set once, [`GateSlot::fill`]); [`Handover`] is the [`Supervisor`] that waits for it.
//!
//! - **Until the slot is filled**: `status` is the idle status, `start_blocked` says "the
//!   proxy is not bound yet", `start` fails with the same words and the other calls say no
//!   update is open. Nothing is built and nothing runs.
//! - **From the first call after it is filled**: a [`SwapSupervisor`] is built over the
//!   host's ports (the first call is `Core::new`'s ask for the status, so a predecessor's
//!   update in flight is resumed before the API serves) and every call goes to it. It is
//!   built once.
//!
//! The ports, one by one: `child`, `gate`, `old_proc`, `releases_dir` and the secret come from
//! the slot; `admin` is [`Admin`] over the two ports of each slot of the upstream table (the
//! secret on every call, the operator token read from its file on each `post_desired`);
//! `recorder` is [`FileRecorder`] over the state directory; `orders` is [`SpoolRootOrders`]
//! over a clone of the one order spool (it shares the counter, and never opens `orders/`);
//! `migrator` is [`StateDbMigrator`], the [`DbMigrator`] over the state database `installer.toml`
//! names, opened at each migration; `clock` is the system clock.
//!
//! `start_blocked` refuses while the start-up hold is in force ([`Host::held`]: no frontend
//! child was adopted, so the active slot is a guess). It does not look at the database: an
//! unreachable one is the refusal of step `migrate` (`database_error`), recorded in the update.
//!
//! Nothing here names the proxy's types: the verb, which does, builds the [`Host`].

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use super::admin::{Admin, Deadlines};
use super::api::traits::{Retired, Supervisor, SupervisorError, SupervisorStatus, UpdateSpec};
use super::child_http::Headers;
use super::handover::HandoverConfig;
use super::migrator::DbMigrator;
use super::model::PortError;
use super::model::{Mode, Phase, ProcIdent, Slot, Timing};
use super::orders::OrderSpool;
use super::ports::{Child, Gate};
use super::ports::{ChildAdmin, Migrator, RootOrders};
use super::record::FileRecorder;
use super::root_orders::SpoolRootOrders;
use super::runtime::{RequestQueue, SystemClock};
use super::supervisor_swap::{Config, Parts, SwapSupervisor};
use crate::CommitId;
use crate::database::Database;
use kbf_state::postgres::schema::{Outcome, SchemaError};
use std::time::Duration;

/// The words of a call made before the proxy is bound.
pub const NOT_BOUND: &str = "proxy not bound yet";

/// The loopback addresses of the two ports of each child, by slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotAddrs {
    /// The admin port of slot A and of slot B.
    pub admin: [SocketAddr; 2],
    /// The operator port of slot A and of slot B.
    pub operator: [SocketAddr; 2],
}

fn index(slot: Slot) -> usize {
    match slot {
        Slot::A => 0,
        Slot::B => 1,
    }
}

impl SlotAddrs {
    #[must_use]
    pub fn admin(&self, slot: Slot) -> SocketAddr {
        self.admin[index(slot)]
    }

    #[must_use]
    pub fn operator(&self, slot: Slot) -> SocketAddr {
        self.operator[index(slot)]
    }
}

/// What the verb knows once the proxy is bound and its children are adopted.
pub struct Host {
    /// The proxy's gate: the hold and the permits.
    pub gate: Arc<dyn Gate + Send + Sync>,
    /// The process control of the children, the same object that adopted them.
    pub child: Arc<dyn Child + Send + Sync>,
    /// The process recorded for a slot, if any, so a restarted proxy can adopt it.
    pub recorded: Arc<dyn Fn(Slot) -> Option<ProcIdent> + Send + Sync>,
    /// The slot that serves now, from the start-up adoption (a recorded update decides its own).
    pub active: Slot,
    /// Why no frontend child is serving (the start-up hold is in force), if so; `active` is
    /// then a guess and no update starts.
    pub held: Option<String>,
    /// `releases/` of the host's layout.
    pub releases_dir: PathBuf,
    /// The headers every admin call carries: the proxy's secret.
    pub secret: Headers,
    /// Stops the process loudly when the swap thread dies.
    pub on_death: Arc<dyn Fn(&str) + Send + Sync>,
}

/// A [`Host`] that is set once, after the proxy is bound. A clone shares the slot.
#[derive(Clone, Default)]
pub struct GateSlot(Arc<OnceLock<Host>>);

impl GateSlot {
    #[must_use]
    pub fn new() -> GateSlot {
        GateSlot::default()
    }

    /// Set the host. True if this call set it; a second call changes nothing and is false.
    pub fn fill(&self, host: Host) -> bool {
        self.0.set(host).is_ok()
    }

    /// Whether the host is set.
    #[must_use]
    pub fn is_filled(&self) -> bool {
        self.0.get().is_some()
    }

    fn get(&self) -> Option<&Host> {
        self.0.get()
    }
}

impl std::fmt::Debug for GateSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GateSlot(filled: {})", self.is_filled())
    }
}

/// What a test replaces of the swap's ports; the verb never sets either.
#[derive(Clone, Default)]
pub(super) struct Seams {
    /// Runs the migration in place of the database.
    pub(super) apply: Option<ApplyHook>,
    /// The admin client in place of [`Admin`] over the children's ports.
    pub(super) admin: Option<Arc<dyn ChildAdmin + Send + Sync>>,
}

/// The supervisor that is the swap once the proxy is bound. See the module documentation.
pub struct Handover {
    cfg: HandoverConfig,
    spool: OrderSpool,
    seams: Seams,
    inner: Mutex<Option<SwapSupervisor>>,
}

impl Handover {
    /// `spool` is the clone of the spool the API writes with.
    #[must_use]
    pub fn new(cfg: &HandoverConfig, spool: OrderSpool) -> Handover {
        Handover {
            cfg: cfg.clone(),
            spool,
            seams: Seams::default(),
            inner: Mutex::new(None),
        }
    }

    /// A handover whose ports are replaced by `seams`.
    #[cfg(test)]
    pub(super) fn with_seams(cfg: &HandoverConfig, spool: OrderSpool, seams: Seams) -> Handover {
        Handover {
            seams,
            ..Handover::new(cfg, spool)
        }
    }

    /// Run `call` on the swap supervisor, building it first if the slot has just been filled;
    /// `None` while the proxy is not bound.
    fn with<R>(&self, call: impl FnOnce(&mut SwapSupervisor) -> R) -> Option<R> {
        let mut inner: MutexGuard<'_, Option<SwapSupervisor>> =
            self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.is_none() {
            let host = self.cfg.gate.get()?;
            *inner = Some(build(&self.cfg, &self.spool, host, &self.seams));
        }
        inner.as_mut().map(call)
    }
}

/// The headers every admin call carries, from the proxy's own: the secret and nothing else the
/// proxy sets for an admin call (no identity). A header the client would refuse is left out;
/// the proxy sets none.
#[must_use]
pub fn admin_secret(proxy: &hyper::HeaderMap) -> Headers {
    proxy.iter().fold(Headers::new(), |all, (name, value)| {
        all.clone()
            .with(name.as_str(), value.as_bytes())
            .unwrap_or(all)
    })
}

/// The client of the children's admin and operator ports: each call goes to the port of its
/// slot with `secret`, and the operator token is read from `token_file` on every offer.
fn admin_of(slots: SlotAddrs, token_file: PathBuf, secret: Headers) -> Admin {
    Admin::new(
        move |slot| slots.admin(slot),
        move |slot| slots.operator(slot),
        secret,
        move || std::fs::read(&token_file),
        Deadlines::frontend(),
    )
}

/// What runs the migration in place of the database: only a test sets one, to see what it is
/// handed instead of reaching a server.
pub(super) type ApplyHook =
    Arc<dyn Fn(&Database, u32, Duration) -> Result<Outcome, SchemaError> + Send + Sync>;

/// The swap's [`Migrator`]: a [`DbMigrator`] over the database `installer.toml` names, built
/// for each migration (the swap runs this once per update, and the proxy never holds a
/// connection or reads the file before it is asked). A file that cannot be read is the
/// refusal `database_error: ...` of step `migrate`, with [`DatabaseError`](crate::database::DatabaseError)'s
/// words (they carry the path, never a password: the file may not carry one).
pub struct StateDbMigrator {
    orders: Arc<dyn RootOrders + Send + Sync>,
    installer_toml: PathBuf,
    apply: Option<ApplyHook>,
}

impl StateDbMigrator {
    #[must_use]
    pub fn new(orders: Arc<dyn RootOrders + Send + Sync>, installer_toml: PathBuf) -> Self {
        StateDbMigrator {
            orders,
            installer_toml,
            apply: None,
        }
    }
}

impl Migrator for StateDbMigrator {
    fn migrate_forward(&self, release: &CommitId) -> Result<(), PortError> {
        let db = Database::from_installer_toml(&self.installer_toml)
            .map_err(|e| PortError(format!("database_error: {e}")))?;
        let migrator = DbMigrator::new(db, &*self.orders);
        match &self.apply {
            None => migrator.migrate_forward(release),
            Some(apply) => {
                let apply = Arc::clone(apply);
                migrator
                    .with_apply(Box::new(move |db, schema, wait| apply(db, schema, wait)))
                    .migrate_forward(release)
            }
        }
    }
}

/// The swap supervisor over `host`'s ports. Resumes an update that was in flight.
fn build(cfg: &HandoverConfig, spool: &OrderSpool, host: &Host, seams: &Seams) -> SwapSupervisor {
    let orders = Arc::new(SpoolRootOrders::new(spool.clone(), &cfg.status_file));
    let admin: Arc<dyn ChildAdmin + Send + Sync> = seams.admin.clone().unwrap_or_else(|| {
        Arc::new(admin_of(
            cfg.slots,
            cfg.operator_token_file.clone(),
            host.secret.clone(),
        ))
    });
    let migrator = StateDbMigrator {
        apply: seams.apply.clone(),
        ..StateDbMigrator::new(orders.clone(), cfg.installer_toml.clone())
    };
    let recorded = Arc::clone(&host.recorded);
    let parts = Parts {
        child: Arc::clone(&host.child),
        admin,
        gate: Arc::clone(&host.gate),
        migrator: Arc::new(migrator),
        recorder: Arc::new(FileRecorder::new(&cfg.state_dir)),
        orders,
        clock: Arc::new(SystemClock::new()),
        requests: Arc::new(RequestQueue::new()),
        old_proc: Box::new(move |slot| recorded(slot)),
        on_death: Arc::clone(&host.on_death),
    };
    SwapSupervisor::new(
        parts,
        Config {
            mode: Mode::Frontend,
            timing: Timing::frontend(),
            state_dir: cfg.state_dir.clone(),
            active: host.active,
            rollback_after_switch: false,
            releases_dir: host.releases_dir.clone(),
            startup_hold: host.held.clone(),
        },
    )
}

impl Supervisor for Handover {
    fn start_blocked(&self) -> Option<String> {
        self.with(|swap| swap.start_blocked())
            .unwrap_or_else(|| Some(NOT_BOUND.to_owned()))
    }

    fn start(&mut self, spec: &UpdateSpec) -> Result<(), SupervisorError> {
        self.with(|swap| swap.start(spec))
            .unwrap_or_else(|| Err(SupervisorError::Failed(NOT_BOUND.to_owned())))
    }

    fn abort(&mut self) -> Result<Phase, SupervisorError> {
        self.with(|swap| swap.abort())
            .unwrap_or(Err(SupervisorError::NotOpen))
    }

    fn rollback(&mut self) -> Result<Phase, SupervisorError> {
        self.with(|swap| swap.rollback())
            .unwrap_or(Err(SupervisorError::NotOpen))
    }

    fn force_retire(&mut self, reason: &str) -> Result<Retired, SupervisorError> {
        self.with(|swap| swap.force_retire(reason))
            .unwrap_or(Err(SupervisorError::NotOpen))
    }

    fn status(&self) -> SupervisorStatus {
        self.with(|swap| swap.status()).unwrap_or_default()
    }
}

#[cfg(test)]
#[path = "handover_swap_tests.rs"]
mod tests;
