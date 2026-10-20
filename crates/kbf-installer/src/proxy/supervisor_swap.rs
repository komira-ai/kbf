//! The handover driver: the real [`Supervisor`] of the update API, over the swap.
//!
//! # The seam
//!
//! [`SwapSupervisor::new`] is the one constructor the proxy verb calls to put a real swap
//! behind `POST /v1/update/frontend` in place of a supervisor that refuses. It takes the
//! ports of the swap ([`Parts`]: the children, the admin client, the proxy's gate, the
//! migrator, the recorder, the root half's orders, the clock) and a [`Config`]; it keeps
//! the [`Supervisor`] trait of the API unchanged.
//!
//! Which port is which, on a host:
//! - `child`: [`super::child_real::ProcChild`];
//! - `admin`: [`super::admin::Admin`] (`admin_protocol` 1, the proxy secret on every call);
//! - `gate`: [`super::gate_real::ProxyGate`];
//! - `migrator`: [`super::migrator::DbMigrator`] (the handover driver wraps it as
//!   `StateDbMigrator`); [`ExpandOnlyDefault`] is the one that migrates nothing, for a test;
//! - `recorder`: [`super::record::FileRecorder`] over the proxy's state directory;
//! - `orders`: [`super::root_orders::SpoolRootOrders`] over the order spool the API writes
//!   with, so the `confirm` and `revert` the swap sends take the same counter;
//! - `clock`: [`super::runtime::SystemClock`].
//!
//! # What it does
//!
//! `start` builds a [`super::Plan`] from the [`UpdateSpec`] and runs [`Swap::begin`] then
//! [`Swap::run`] on a thread of its own, named `kbf-swap`: the swap blocks on admin calls
//! and sleeps, and the API's actor must not. It writes the first intent record before it
//! returns, so the status shows the update from the moment `start` answers. `start` is
//! idempotent on the update id.
//!
//! The status is assembled from the intent record the swap thread wrote last (kept in
//! memory beside `update.json`, never ahead of it), the proxy's permits on each slot and
//! the old child's own counts; see the `status` module.
//!
//! `abort` queues a [`super::model::Request::Abort`] and answers with the phase at the time
//! of the request; the swap decides at its next poll whether it can honor it.
//! `force_retire` sends SIGTERM to the draining old child through the `child` port; its
//! open streams end when it exits, and the swap's own drain then finishes.
//!
//! # Restart
//!
//! [`SwapSupervisor::new`] reads `update.json`. An update that was in flight is taken up
//! again with [`Swap::adopt`] on a new swap thread (the children outlive the proxy), and
//! resumes by phase; the record is the source of truth for the swap's phase, whatever the
//! API's request map says. An unreadable record makes every `start` refuse
//! (`refused`) and leaves both children as they are. Nothing is held for a finished update.
//!
//! # What does not work, stated
//!
//! - **Rollback after the switch is refused.** The swap stands the old child down at the
//!   switch, and a child that stood down never serves again (`stand-down` is never
//!   reversed; `resume` answers 409). A rollback would stand the new child down and fail to
//!   bring the old one back. [`Config::rollback_after_switch`] stays off until the child can
//!   resume after a stand-down; the swap's own rollback is tested against fake children only.
//! - **Both children dead on a cold start** needs the child to bump its epoch, which
//!   `--await-takeover` does not do: the swap records `restart` and fails.
//! - **A thread that dies takes the proxy with it** ([`Parts::on_death`]); the restart goes
//!   through the adopt path above, with the startup hold in force until it decides.

mod migrator;
mod mirror;
mod status;

#[cfg(test)]
mod blocked_tests;
#[cfg(test)]
mod core_tests;
#[cfg(test)]
mod migrator_tests;
#[cfg(test)]
pub(in crate::proxy) mod rig;
#[cfg(test)]
mod tests;

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use super::api::traits::{Retired, Supervisor, SupervisorError, SupervisorStatus, UpdateSpec};
use super::model::{
    Failure, Halt, Intent, Mode, Phase, Plan, ProcIdent, Request, Slot, Step, Timing,
};
use super::ports::{
    Child, ChildAdmin, Clock, Gate, Migrator, Ports, Recorder, Requests, RootOrders,
};
use super::record::FileRecorder;
use super::runtime::RequestQueue;
use super::swap::Swap;
use mirror::{Mirror, Shared};

pub use migrator::{ExpandOnlyDefault, RunningSchema};

/// The ports of the swap, owned: the swap thread borrows them for as long as it runs.
pub struct Parts {
    pub child: Arc<dyn Child + Send + Sync>,
    pub admin: Arc<dyn ChildAdmin + Send + Sync>,
    pub gate: Arc<dyn Gate + Send + Sync>,
    pub migrator: Arc<dyn Migrator + Send + Sync>,
    pub recorder: Arc<dyn Recorder + Send + Sync>,
    pub orders: Arc<dyn RootOrders + Send + Sync>,
    pub clock: Arc<dyn Clock + Send + Sync>,
    /// Where `abort` goes; the swap reads it.
    pub requests: Arc<RequestQueue>,
    /// The process identity of the child serving in a slot, recorded in `update.json` so a
    /// restarted proxy can adopt it (`ProcChild::recorded` on a host).
    pub old_proc: Box<dyn Fn(Slot) -> Option<ProcIdent> + Send + Sync>,
    /// Runs once on the swap thread when it dies of a panic or a port it cannot do without;
    /// the verb passes what stops its process loudly. Not run for a shutdown.
    pub on_death: Arc<dyn Fn(&str) + Send + Sync>,
}

/// What the supervisor is told besides its ports.
#[derive(Clone, Debug)]
pub struct Config {
    pub mode: Mode,
    pub timing: Timing,
    /// The proxy's state directory: `update.json` is read from it at the start.
    pub state_dir: PathBuf,
    /// The slot that serves now, from the start-up adoption. A recorded update that is
    /// still open decides its own.
    pub active: Slot,
    /// Allow `rollback` after the switch. Off: see the module's "what does not work".
    pub rollback_after_switch: bool,
    /// Where the releases are installed (`releases/` of the host's layout): a candidate
    /// cannot start when it is not a directory ([`Supervisor::start_blocked`]).
    pub releases_dir: PathBuf,
    /// Set while the start-up hold is in force (no frontend child was adopted): why. `active`
    /// is then a guess, so no update can start ([`Supervisor::start_blocked`]).
    pub startup_hold: Option<String>,
}

/// Starts the swap's thread. The real one is [`thread_spawn`]; a test passes one that refuses.
type Spawn = Box<dyn Fn(Box<dyn FnOnce() + Send>) -> Result<(), String> + Send + Sync>;

fn thread_spawn() -> Spawn {
    Box::new(|work| {
        thread::Builder::new()
            .name("kbf-swap".into())
            .spawn(work)
            .map(|_| ())
            .map_err(spawn_error)
    })
}

fn spawn_error(e: std::io::Error) -> String {
    format!("the swap thread could not start: {e}")
}

/// The [`Supervisor`] over the swap. See the module documentation.
pub struct SwapSupervisor {
    parts: Arc<Parts>,
    spawn: Spawn,
    shared: Arc<Shared>,
    config: Config,
    /// `update.json` could not be read at the start: the reason.
    unreadable: Option<String>,
}

enum Job {
    Fresh {
        plan: Plan,
        old_proc: Option<ProcIdent>,
    },
    Adopt {
        plan: Plan,
        intent: Box<Intent>,
    },
}

impl SwapSupervisor {
    /// Build the supervisor and, if `update.json` shows an update in flight, resume it on a
    /// swap thread.
    #[must_use]
    pub fn new(parts: Parts, config: Config) -> SwapSupervisor {
        SwapSupervisor::with_spawner(parts, config, thread_spawn())
    }

    fn with_spawner(parts: Parts, config: Config, spawn: Spawn) -> SwapSupervisor {
        let parts = Arc::new(parts);
        let (intent, unreadable) = match FileRecorder::load(&config.state_dir) {
            Ok(intent) => (intent, None),
            Err(e) => (None, Some(e.to_string())),
        };
        let active = intent
            .as_ref()
            .map_or(config.active, |i| status::active_after(i, &|| false));
        let resume = intent.clone().filter(|i| !i.phase.is_terminal());
        let open = resume.as_ref().map(|i| i.update_id.clone());
        let me = SwapSupervisor {
            parts,
            spawn,
            shared: Arc::new(Shared::new(active, intent, open)),
            config,
            unreadable,
        };
        if let Some(intent) = resume {
            let plan = Plan::resumed(&intent, me.config.timing);
            if let Err(why) = me.launch(Job::Adopt {
                plan,
                intent: Box::new(intent),
            }) {
                // The death is announced while the update still reads as open, as a swap
                // thread's own is: whatever stops the process does so before a status says
                // the update is over.
                (me.parts.on_death)(&why);
                me.shared.close();
            }
        }
        me
    }

    /// Wait until no swap thread is alive, for at most `limit`; true if none is.
    pub fn wait_closed(&self, limit: Duration) -> bool {
        self.shared.wait_closed(limit)
    }

    fn launch(&self, job: Job) -> Result<(), String> {
        let parts = Arc::clone(&self.parts);
        let shared = Arc::clone(&self.shared);
        (self.spawn)(Box::new(move || work(&parts, &shared, job)))
    }

    /// The phase of the open update, or `NotOpen`.
    fn open_phase(&self) -> Result<Phase, SupervisorError> {
        let state = self.shared.lock();
        match (&state.open, &state.intent) {
            (Some(_), Some(intent)) => Ok(intent.phase),
            _ => Err(SupervisorError::NotOpen),
        }
    }

    /// The swap thread never started: the update ends `failed` at `start`.
    fn launch_failed(&self, mut intent: Intent, why: &str) {
        let mut state = self.shared.lock();
        state.open = None;
        intent.phase = Phase::Failed;
        intent.failure = Some(Failure {
            step: Step::Start,
            reason: why.to_owned(),
        });
        if self.parts.recorder.record(&intent).is_ok() {
            state.intent = Some(intent);
        }
    }
}

/// The swap thread: run the job, record where it ended, and say loudly if it died.
fn work(parts: &Parts, shared: &Shared, job: Job) {
    let death = match catch_unwind(AssertUnwindSafe(|| run(parts, shared, job))) {
        Ok(Ok(_) | Err(Halt::Interrupted)) => None,
        Ok(Err(halt)) => Some(format!("the swap stopped: {halt}")),
        Err(panic) => Some(format!("the swap thread panicked: {}", panic_text(&*panic))),
    };
    if let Some(why) = &death {
        tracing::error!(%why, "the handover driver died");
        (parts.on_death)(why);
    }
    shared.close();
}

fn run(parts: &Parts, shared: &Shared, job: Job) -> Result<Phase, Halt> {
    let recorder = Mirror {
        inner: &*parts.recorder,
        shared,
    };
    let ports = Ports {
        child: &*parts.child,
        admin: &*parts.admin,
        gate: &*parts.gate,
        migrator: &*parts.migrator,
        recorder: &recorder,
        orders: &*parts.orders,
        clock: &*parts.clock,
        requests: &*parts.requests,
    };
    let mut swap = match job {
        Job::Fresh { plan, old_proc } => Swap::begin(ports, plan, old_proc)?,
        Job::Adopt { plan, intent } => Swap::adopt(ports, plan, *intent),
    };
    let phase = swap.run()?;
    let intent = swap.intent().clone();
    let new = intent.new_slot;
    let holds = || parts.admin.role(new).is_ok_and(|r| r.holder);
    shared.lock().active = status::active_after(&intent, &holds);
    Ok(phase)
}

fn panic_text(panic: &(dyn Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "no message".to_owned())
}

impl Supervisor for SwapSupervisor {
    /// Why a `POST` cannot be taken now, asked before anything is saved or written (so the
    /// root half does not fetch a release nothing can take over). In this order:
    /// - `update.json` is unreadable (every `start` would refuse);
    /// - an update that has ended still has its swap thread open (it is draining the old
    ///   child): the `start` of a new one would be refused after its release was fetched;
    /// - the releases directory is missing;
    /// - the start-up hold is in force (no frontend child was adopted, so the active slot is a
    ///   guess);
    /// - the slot the candidate would start in is not idle: the process recorded for it is
    ///   still running.
    ///
    /// An update that is still going on is **not** reported here: the API refuses that
    /// `POST` itself with `409 update_open` (and the busy slot of its candidate with it), which
    /// `deploy` reads as "busy, not now" (exit 4); a `503` from here would read as a failure.
    fn start_blocked(&self) -> Option<String> {
        if let Some(why) = &self.unreadable {
            return Some(format!(
                "update.json is unreadable ({why}); both children are left as they are"
            ));
        }
        let (open, intent, active) = {
            let state = self.shared.lock();
            (state.open.clone(), state.intent.clone(), state.active)
        };
        if let Some(open) = open {
            return if intent.is_some_and(|i| i.phase.is_terminal()) {
                Some(format!("update {open} has ended and is still finishing"))
            } else {
                None
            };
        }
        if !self.config.releases_dir.is_dir() {
            return Some(format!(
                "the releases directory {} is missing",
                self.config.releases_dir.display()
            ));
        }
        if let Some(why) = &self.config.startup_hold {
            return Some(format!(
                "no frontend child is serving; the startup hold is in force: {why}"
            ));
        }
        let idle = active.other();
        match (self.parts.old_proc)(idle) {
            Some(ident) if self.parts.child.exited(&ident).is_none() => Some(format!(
                "slot {idle:?} is not idle: process {} still runs in it",
                ident.pid
            )),
            _ => None,
        }
    }

    fn start(&mut self, spec: &UpdateSpec) -> Result<(), SupervisorError> {
        if let Some(why) = &self.unreadable {
            return Err(SupervisorError::Refused(format!(
                "update.json is unreadable ({why}); both children are left as they are"
            )));
        }
        let id = spec.update_id.0.clone();
        let mut state = self.shared.lock();
        if state.open.as_ref() == Some(&id)
            || state.intent.as_ref().is_some_and(|i| i.update_id == id)
        {
            return Ok(());
        }
        if let Some(open) = &state.open {
            return Err(SupervisorError::Refused(format!(
                "update {open} is still open"
            )));
        }
        let from = spec.from.clone().ok_or_else(|| {
            SupervisorError::Refused("the running commit is not known".to_owned())
        })?;
        let plan = Plan {
            update_id: id.clone(),
            request_id: spec.request_id.clone(),
            mode: self.config.mode,
            from,
            to: spec.to.clone(),
            nodes: spec.nodes,
            rollback: spec.rollback,
            old_slot: state.active,
            timing: self.config.timing,
        };
        let old_proc = (self.parts.old_proc)(state.active);
        let intent = Intent::begin(&plan, old_proc.clone(), self.parts.clock.now().as_secs());
        self.parts
            .recorder
            .record(&intent)
            .map_err(|e| SupervisorError::Failed(e.to_string()))?;
        // A request left from an update that ended is not this update's.
        while self.parts.requests.next().is_some() {}
        state.intent = Some(intent.clone());
        state.open = Some(id);
        drop(state);
        if let Err(why) = self.launch(Job::Fresh { plan, old_proc }) {
            self.launch_failed(intent, &why);
            return Err(SupervisorError::Failed(why));
        }
        Ok(())
    }

    fn abort(&mut self) -> Result<Phase, SupervisorError> {
        let phase = self.open_phase()?;
        self.parts.requests.push(Request::Abort);
        Ok(phase)
    }

    fn rollback(&mut self) -> Result<Phase, SupervisorError> {
        let phase = self.open_phase()?;
        if !self.config.rollback_after_switch {
            return Err(SupervisorError::Refused(
                "rollback after the switch is not supported: the old child stood down at the \
                 switch and a child that stood down cannot serve again"
                    .to_owned(),
            ));
        }
        self.parts.requests.push(Request::Rollback);
        Ok(phase)
    }

    fn force_retire(&mut self, reason: &str) -> Result<Retired, SupervisorError> {
        let intent = {
            let state = self.shared.lock();
            state.intent.clone().filter(|_| state.open.is_some())
        };
        let Some(intent) = intent.filter(status::has_old_phase) else {
            return Err(SupervisorError::NotOpen);
        };
        let slot = status::draining_slot(&intent);
        let ident = if slot == intent.old_slot {
            intent.old_proc.clone()
        } else {
            intent.new_proc.clone()
        };
        let Some(ident) = ident else {
            return Err(SupervisorError::Refused(
                "the old child is not recorded".into(),
            ));
        };
        if self.parts.child.exited(&ident).is_some() {
            return Err(SupervisorError::Refused("the old child is gone".into()));
        }
        let dropped = self.parts.gate.open_streams(slot);
        self.parts
            .child
            .terminate(&ident)
            .map_err(|e| SupervisorError::Failed(format!("SIGTERM to {slot:?}: {e}")))?;
        tracing::warn!(reason, dropped, ?slot, "old child told to end");
        Ok(Retired {
            dropped_streams: dropped,
        })
    }

    fn status(&self) -> SupervisorStatus {
        let (intent, active, open) = {
            let state = self.shared.lock();
            (state.intent.clone(), state.active, state.open.is_some())
        };
        status::assemble(&self.parts, intent, active, open)
    }
}
