//! The seams of the swap. Each trait is one thing the swap needs from outside and that a
//! later unit builds for real: the process side (`Child`), the child's admin calls
//! (`ChildAdmin`, frozen as `admin_protocol`), the proxy's hold and permits (`Gate`, U4b),
//! forward migrations (`Migrator`), `update.json` (`Recorder`), the root half's orders
//! (`RootOrders`), time (`Clock`) and operator requests (`Requests`).
//!
//! Every call is synchronous. A real implementation blocks on its own runtime; the swap
//! never holds a lock across a port call. The only way the swap waits is `Clock::sleep`.

use std::time::Duration;

use crate::CommitId;

use super::model::{
    AdoptError, CrashPoint, Halt, Intent, Locator, PortError, ProcIdent, Readiness, Request,
    Retired, RoleInfo, Slot, StagedFacts, Start,
};

/// The process side of the children. The proxy starts them as its own processes.
pub trait Child {
    /// Start `releases/<commit>/kbf --mode=frontend` in `slot`, listening on the slot's
    /// ports and, for [`Start::Candidate`], waiting for a takeover. The returned identity is
    /// what the intent record keeps for adoption.
    fn spawn(&self, slot: Slot, release: &CommitId, start: Start) -> Result<ProcIdent, PortError>;

    /// Take over a process recorded before a restart. Refuses a recycled pid: the live
    /// process must match pid, start time and executable ([`super::model::verify_ident`]).
    fn adopt(&self, slot: Slot, recorded: &ProcIdent) -> Result<(), AdoptError>;

    /// The exit code if the process is gone.
    fn exited(&self, ident: &ProcIdent) -> Option<i32>;

    /// SIGTERM. This is the only signal the swap sends a child that has served; there is
    /// no SIGKILL for it and no timeout behind it.
    fn terminate(&self, ident: &ProcIdent) -> Result<(), PortError>;

    /// SIGKILL a candidate that never served (FP section 5 steps 4 and 6). The swap calls
    /// it only on the process it spawned (or adopted) as the candidate, never on the old child.
    fn kill_unserved(&self, ident: &ProcIdent) -> Result<(), PortError>;
}

/// The admin calls on a child (`admin_protocol` 1: `GET role`, `POST quiesce`, `resume`,
/// `takeover`, `stand-down`) and the two reads the swap needs besides (`ready`,
/// `post_desired`), addressed by slot.
///
/// Every call answers 200 with the child's view ([`RoleInfo`]), 409 with an error when it
/// does not apply to the child's role, 400 for a bad body; all of those but the 200 are
/// `Err` here. A real adapter sends the proxy secret header on every call (the admin port
/// will require it), over [`super::child_http`]'s loopback client; it is not built yet (the
/// child's admin port, #1103, is not merged here), so only the fake implements this trait.
pub trait ChildAdmin {
    fn role(&self, slot: Slot) -> Result<RoleInfo, PortError>;

    /// Take the generation: atomically, reload all state, start the schedulers. Candidate
    /// only. `expect` is the generation the holder holds (`held_generation` of its view),
    /// or the store's (`generation` of the candidate's own view) when no child holds it.
    /// 409: the child is not a candidate or the store is not at `expect`, nothing written.
    /// 500: it took the generation and could not start; the child exits. The swap measures
    /// the elapsed time itself.
    fn takeover(&self, slot: Slot, expect: u64) -> Result<RoleInfo, PortError>;

    /// Stop scheduling, assigning and requeueing and flush. Synchronous, up to 10 s: the
    /// view says `Quiesced`. Idempotent. It does not wait for running work.
    fn quiesce(&self, slot: Slot) -> Result<RoleInfo, PortError>;

    fn stand_down(&self, slot: Slot) -> Result<RoleInfo, PortError>;

    /// The inverse of `quiesce` and `stand-down` while the process lives: a full reload
    /// from the store, then a re-take of the generation if the holder is dead or agrees.
    /// Idempotent.
    fn resume(&self, slot: Slot) -> Result<RoleInfo, PortError>;

    fn ready(&self, slot: Slot) -> Result<Readiness, PortError>;

    /// `POST /v1/update/desired {commit}` on the child's operator port; idempotent
    /// (FP section 5 step 9).
    fn post_desired(&self, slot: Slot, commit: &CommitId) -> Result<(), PortError>;
}

/// The proxy's routing state for the update: the hold and the per-slot permits. The hold
/// lives in `kbf-proxy` (U4b); the swap only drives it ([`super::gate_real::ProxyGate`]).
pub trait Gate {
    /// Accept new streams and read their headers but open nothing to a backend, for at
    /// most `max`. The update API, `/healthz`, `/readyz` and metrics bypass it.
    fn hold(&self, max: Duration) -> Result<(), PortError>;

    /// Make `slot` the active slot and release every held stream into it, atomically.
    /// With nothing held it only sets the active slot (a restarted proxy uses it so).
    fn release_into(&self, slot: Slot) -> Result<(), PortError>;

    /// Fail each held stream that has waited `hold_max` with gRPC `UNAVAILABLE` and count
    /// it in the hold-expired counter of the proxy's metrics. Returns how many. Does not end the update.
    /// The swap calls it on every poll of a wait and before it releases the hold.
    fn expire(&self) -> usize;

    /// The proxy's permits on `slot`: client streams bound to it that are still open.
    fn open_streams(&self, slot: Slot) -> u64;

    /// No slot is active; a hold in force stays. The all-in-one swap calls it as soon as
    /// the old process is stopped, so the release that follows (or a failed start) answers
    /// `UNAVAILABLE` at once instead of binding streams to a dead child.
    fn clear_active(&self);

    /// The proxy's commit on `slot`: close it if it is not active and no stream is bound.
    /// The swap signals a child only when this says [`Retired::Closed`], after the child's
    /// own counts are zero. A slot closed here is opened again, under a new generation, by
    /// the next `release_into` it receives.
    fn retire(&self, slot: Slot) -> Retired;
}

/// Forward, backward-compatible Postgres migrations. The kbf child never alters tables
/// (CEO 14:58Z), so this runs before the candidate starts.
///
/// OPEN: who implements it (a root-half order or the proxy) and with which database
/// credential is undecided. The swap needs only this call.
pub trait Migrator {
    /// Apply every forward migration release `release` needs. Idempotent: running it
    /// again after a crash changes nothing.
    fn migrate_forward(&self, release: &CommitId) -> Result<(), PortError>;
}

/// `update.json`: intent, written before each action (Q5).
pub trait Recorder {
    /// Persist `intent` (temp, fsync, rename). An error stops the swap before it acts.
    fn record(&self, intent: &Intent) -> Result<(), Halt>;

    /// A fault-injection hook. The swap calls it at each named crash point; a test
    /// recorder returns `Err(Halt::Crashed(_))` there to kill the proxy at that moment.
    fn crash_point(&self, _point: CrashPoint) -> Result<(), Halt> {
        Ok(())
    }
}

/// The orders to the root half, and the one fact read back from it.
pub trait RootOrders {
    /// Write a `frontend_update` locator (Q1). U8 drives this before the swap starts.
    fn frontend_update(&self, locator: &Locator) -> Result<(), PortError>;

    /// The `staged{}` object of `status.json`, if a release is staged.
    fn staged(&self) -> Result<Option<StagedFacts>, PortError>;

    /// The commit `status.json` reports as `installed_commit`: the root's `current`. After
    /// a `confirm` it names the confirmed commit; it is the one fact that says whether a
    /// confirm landed, whatever `update.json` recorded.
    fn installed(&self) -> Result<Option<CommitId>, PortError>;

    /// Flip `previous` then `current` to the staged commit, after the bake (Q2).
    fn confirm(&self, commit: &CommitId) -> Result<(), PortError>;

    /// Drop the staged release; `current` was never touched (Q2).
    fn revert(&self, commit: &CommitId) -> Result<(), PortError>;
}

/// Time. `now` is the wall clock as a duration since the Unix epoch, so a bake start
/// recorded in `update.json` still means something after a restart.
pub trait Clock {
    fn now(&self) -> Duration;

    /// Wait. `Err(Halt::Interrupted)` when the proxy is shutting down.
    fn sleep(&self, d: Duration) -> Result<(), Halt>;
}

/// Operator requests that arrive while the swap runs. The API actor (U8) queues them.
pub trait Requests {
    /// The next request, if any; asking consumes it.
    fn next(&self) -> Option<Request>;
}

/// The ports one swap uses.
#[derive(Clone, Copy)]
pub struct Ports<'a> {
    pub child: &'a dyn Child,
    pub admin: &'a dyn ChildAdmin,
    pub gate: &'a dyn Gate,
    pub migrator: &'a dyn Migrator,
    pub recorder: &'a dyn Recorder,
    pub orders: &'a dyn RootOrders,
    pub clock: &'a dyn Clock,
    pub requests: &'a dyn Requests,
}
