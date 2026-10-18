//! The role of a frontend process and the transitions between roles.
//!
//! ```text
//! candidate --takeover--> active --quiesce--> quiesced --stand-down--> stood-down
//!                           ^                    |
//!                           +------ resume ------+      active --stand-down--> stood-down
//! ```
//!
//! One transition runs at a time (`moving`); `GET role` reads a snapshot and never waits
//! for one.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::oneshot;

use super::candidate::Candidate;
use crate::frontend::{FrontendFlags, FrontendHandle};
use crate::handover::{Stand, Stop};
use crate::state_store::generation_of;

/// The version of the calls and of the `role` answer, which a proxy checks against
/// `kbf --version --json`. Raised when a call or a field changes meaning.
pub const ADMIN_PROTOCOL: u32 = 1;

/// How often a stood-down process looks in the state store for the results of the calls
/// it kept open.
const FOLLOW_EVERY: Duration = Duration::from_millis(200);

/// What a process is, as far as the store's generation goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    /// Waits for a takeover: holds no generation and schedules nothing.
    Candidate,
    /// Holds the generation and schedules.
    Active,
    /// Holds the generation and has stopped scheduling, assigning and requeueing, for a
    /// takeover that may not happen: [`Control::resume`] undoes it.
    Quiesced,
    /// Another process holds the generation: this one ended its worker streams, answers
    /// the calls it kept from `done/` rows and is not ready.
    StoodDown,
}

/// The answer to `GET role`, and what a transition answers with.
#[derive(Clone, Debug, Serialize)]
pub struct RoleView {
    /// The role.
    pub role: Role,
    /// The UUID of this process.
    pub instance_id: String,
    /// [`ADMIN_PROTOCOL`].
    pub admin_protocol: u32,
    /// The generation the state store is at now, if it can be read: what a takeover
    /// passes as `expect`.
    pub generation: Option<u64>,
    /// The generation this process took; none for a candidate.
    pub held_generation: Option<u64>,
    /// The store has refused a write of this process because another writer holds it.
    pub superseded: bool,
    /// Execute and WaitExecution streams open.
    pub open_streams: u64,
    /// Worker streams open.
    pub open_sessions: usize,
    /// The REAPI listener.
    pub reapi: Option<SocketAddr>,
    /// The worker listener; none until this process is the writer, and none again once
    /// it stood down.
    pub worker: Option<SocketAddr>,
    /// The operator API listener.
    pub api: Option<SocketAddr>,
}

/// Why a call changed nothing.
#[derive(Debug)]
pub enum Refusal {
    /// The call does not apply to the role the process has; the current view.
    Role(Box<RoleView>, &'static str),
    /// The store is not at the generation the call expected, or another writer took it.
    Generation(String),
    /// The transition failed after it began; the process cannot continue as it was.
    Failed(String),
}

/// What the process serves the real frontend through: the handle a takeover built, or why
/// it could not.
pub(crate) type Handoff = oneshot::Sender<Result<Box<FrontendHandle>, String>>;

struct Running {
    stand: Stand,
    /// The generation this process took.
    held: u64,
}

struct Inner {
    role: Role,
    candidate: Option<Candidate>,
    running: Option<Running>,
    reapi: Option<SocketAddr>,
    worker: Option<SocketAddr>,
    api: Option<SocketAddr>,
}

/// The role of this process.
pub struct Control {
    id: String,
    flags: FrontendFlags,
    stop: Stop,
    inner: Mutex<Inner>,
    moving: tokio::sync::Mutex<()>,
    /// The follower of the kept calls runs ([`follow`]): once, whichever of a stand-down
    /// and the first refused write starts it.
    following: Arc<AtomicBool>,
    /// Where the handle of the real frontend goes once a takeover has built it.
    handoff: Mutex<Option<Handoff>>,
}

impl Control {
    pub(crate) fn new(id: String, flags: FrontendFlags, stop: Stop, handoff: Handoff) -> Self {
        Self {
            id,
            flags,
            stop,
            inner: Mutex::new(Inner {
                role: Role::Candidate,
                candidate: None,
                running: None,
                reapi: None,
                worker: None,
                api: None,
            }),
            moving: tokio::sync::Mutex::new(()),
            following: Arc::new(AtomicBool::new(false)),
            handoff: Mutex::new(Some(handoff)),
        }
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn set_candidate(&self, candidate: Candidate) {
        let mut inner = self.inner();
        inner.reapi = Some(candidate.reapi);
        inner.api = candidate.api;
        inner.candidate = Some(candidate);
    }

    /// This process serves as the writer, through `handle`: it holds the generation the
    /// store is at.
    pub(crate) fn set_running(&self, handle: &FrontendHandle) {
        let mut inner = self.inner();
        inner.role = Role::Active;
        inner.reapi = Some(handle.reapi);
        inner.worker = Some(handle.worker);
        inner.api = handle.api;
        inner.running = handle.stand().cloned().map(|stand| {
            watch(Arc::clone(&stand.farm), Arc::clone(&self.following));
            // A store that cannot be read now is a held generation of 0, which no store
            // is at: a resume is refused.
            let held = generation_of(stand.store.as_ref()).unwrap_or(0);
            Running { stand, held }
        });
    }

    /// The role, as of now.
    #[must_use]
    pub fn view(&self) -> RoleView {
        let inner = self.inner();
        let candidate = inner.candidate.as_ref();
        let (held, generation, farm) = match &inner.running {
            Some(run) => (
                Some(run.held),
                generation_of(run.stand.store.as_ref()).ok(),
                Some(Arc::clone(&run.stand.farm)),
            ),
            None => (
                None,
                candidate.and_then(|candidate| candidate.store.generation().ok()),
                None,
            ),
        };
        RoleView {
            role: inner.role,
            instance_id: self.id.clone(),
            admin_protocol: ADMIN_PROTOCOL,
            generation,
            held_generation: held,
            superseded: farm.as_ref().is_some_and(|farm| farm.superseded()),
            open_streams: farm.as_ref().map_or(0, |farm| farm.open_executions()),
            open_sessions: farm.as_ref().map_or(0, |farm| farm.open_sessions()),
            reapi: inner.reapi,
            worker: inner.worker,
            api: inner.api,
        }
    }

    fn wrong(&self, wanted: &'static str) -> Refusal {
        Refusal::Role(Box::new(self.view()), wanted)
    }

    /// The running state, if the role is one of `roles`.
    fn running_in(&self, roles: &[Role]) -> Option<(Role, Stand, u64)> {
        let inner = self.inner();
        let run = inner.running.as_ref()?;
        roles
            .contains(&inner.role)
            .then(|| (inner.role, run.stand.clone(), run.held))
    }

    fn set_role(&self, role: Role) {
        let mut inner = self.inner();
        inner.role = role;
        if role == Role::StoodDown {
            inner.worker = None;
        }
    }

    /// Stops scheduling, assigning and requeueing, and writes the requeues the store
    /// refused. An active process becomes quiesced; a quiesced one stays so.
    ///
    /// # Errors
    /// [`Refusal::Role`]: this process is a candidate or stood down.
    pub async fn quiesce(&self) -> Result<RoleView, Refusal> {
        let _moving = self.moving.lock().await;
        let Some((role, stand, _)) = self.running_in(&[Role::Active, Role::Quiesced]) else {
            return Err(self.wrong("active or quiesced"));
        };
        if role == Role::Active {
            stand.quiesce().await;
            self.set_role(Role::Quiesced);
        }
        Ok(self.view())
    }

    /// Puts a quiesced process back to work, after a takeover that did not happen: only
    /// while the store is still at the generation this process took. The farm fed the
    /// scheduler nothing while it was quiesced, so its memory is the store's as of the
    /// quiesce.
    ///
    /// # Errors
    /// [`Refusal::Role`]: not quiesced or active. [`Refusal::Generation`]: another writer
    /// holds the generation now; this process can only stand down. [`Refusal::Failed`]:
    /// the store cannot be read.
    pub async fn resume(&self) -> Result<RoleView, Refusal> {
        let _moving = self.moving.lock().await;
        let Some((role, stand, held)) = self.running_in(&[Role::Active, Role::Quiesced]) else {
            return Err(self.wrong("quiesced or active"));
        };
        if role == Role::Quiesced {
            let now = generation_of(stand.store.as_ref()).map_err(Refusal::failed)?;
            if now != held {
                return Err(Refusal::Generation(format!(
                    "the store is at generation {now}, not the {held} this process took: \
                     another writer holds it, stand down"
                )));
            }
            stand.farm.resume();
            self.set_role(Role::Active);
        }
        Ok(self.view())
    }

    /// Ends the worker streams (`UNAVAILABLE`, connect again), stops the worker
    /// listener, answers the calls it kept from `done/` rows and goes not ready. The
    /// REAPI listener sends no GOAWAY. Never reversed.
    ///
    /// # Errors
    /// [`Refusal::Role`]: this process is a candidate.
    pub async fn stand_down(&self) -> Result<RoleView, Refusal> {
        let _moving = self.moving.lock().await;
        let Some((role, stand, _)) =
            self.running_in(&[Role::Active, Role::Quiesced, Role::StoodDown])
        else {
            return Err(self.wrong("active, quiesced or stood-down"));
        };
        if role != Role::StoodDown {
            stand.release().await;
            self.set_role(Role::StoodDown);
            follow(Arc::clone(&stand.farm), &self.following);
        }
        Ok(self.view())
    }

    /// Takes the generation, which the store must be at exactly as `expect` says, keeps
    /// the epoch, reloads all state from the store and starts the scheduler, the
    /// reconciler and the worker listener.
    ///
    /// # Errors
    /// [`Refusal::Role`]: not a candidate. [`Refusal::Generation`]: the store is at
    /// another generation (nothing is written). [`Refusal::Failed`]: the generation was
    /// taken and the frontend could not start; the process ends.
    pub async fn takeover(&self, expect: u64) -> Result<RoleView, Refusal> {
        let _moving = self.moving.lock().await;
        let (state, candidate) = {
            let mut inner = self.inner();
            let Some(candidate) = inner.candidate.take() else {
                drop(inner);
                return Err(self.wrong("candidate"));
            };
            match candidate.store.take_over(expect) {
                Ok(state) => (state, candidate),
                Err(error) => {
                    inner.candidate = Some(candidate);
                    return Err(Refusal::Generation(error.to_string()));
                }
            }
        };
        let (sockets, _store) = candidate.retire().await;
        let started =
            crate::frontend::start_over_sockets(&self.flags, sockets, state, self.stop.clone());
        let handoff = self
            .handoff
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        match started {
            Ok(handle) => {
                self.set_running(&handle);
                let _ = handoff.map(|handoff| handoff.send(Ok(Box::new(handle))));
                Ok(self.view())
            }
            Err(error) => {
                let why = error.to_string();
                tracing::error!(error = %why, "the generation was taken and the frontend did not start");
                let _ = handoff.map(|handoff| handoff.send(Err(why.clone())));
                Err(Refusal::Failed(why))
            }
        }
    }
}

impl Refusal {
    fn failed(error: impl std::fmt::Display) -> Self {
        Self::Failed(error.to_string())
    }
}

/// Answers the calls a farm kept from the `done/` rows another writer makes
/// ([`crate::farm::Farm::follow`]), every [`FOLLOW_EVERY`], for as long as the process
/// lives; started once per process.
fn follow(farm: Arc<crate::farm::Farm>, started: &AtomicBool) {
    if started.swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::spawn(async move {
        let mut look = tokio::time::interval(FOLLOW_EVERY);
        loop {
            look.tick().await;
            // A store that cannot be read is looked at again at the next look; the farm
            // reports what it finds no trace of (`kbf_frontend_relay_untraced`).
            let _ = farm.follow();
        }
    });
}

/// Waits for the state store to refuse a write of `farm` because another writer holds the
/// generation, and then does what a stand-down does for the calls it holds: fences the
/// farm (nothing more is fed to the scheduler) and follows the `done/` rows, so a caller
/// waiting on Execute or WaitExecution is answered by the new writer's result instead of
/// hanging. A process that stands down first has the follower already.
fn watch(farm: Arc<crate::farm::Farm>, started: Arc<AtomicBool>) {
    tokio::spawn(async move {
        let mut look = tokio::time::interval(FOLLOW_EVERY);
        while !farm.superseded() {
            look.tick().await;
        }
        tracing::warn!("the state store has a newer writer: following its results");
        farm.fence();
        follow(farm, &started);
    });
}
