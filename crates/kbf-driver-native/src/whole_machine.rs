//! [`WholeMachineRuntime`]: the bare-metal whole-machine runtime for Macs, which serves
//! `whole_machine` leases as driver `native-whole-machine`
//! (`docs/design/fleet-updates.md` sections 10.1 and 10.2), GPU tests among them.
//!
//! Each lease runs as its own throwaway user, in that user's GUI session, through the
//! root helper `kbf-mac-session` ([`Session`]):
//!
//! 1. **Preparing.** `user-create`, then `session-login` (auto-login into the user and
//!    a userspace restart, which ends the daemon too), then `session-ready` every
//!    [`WholeMachineConfig::session_poll`] until launchd has the user's `gui/<uid>`
//!    domain, for at most [`WholeMachineConfig::session_wait`] counted from when the
//!    lease was first taken.
//! 2. **The action**, in that session (`run` with the GUI target): its inputs written
//!    into a lease directory the lease user may work in, its `arguments` run there with
//!    the Command's environment, stdout and stderr captured, its outputs read back
//!    with `kbf-outputs`; a wall-clock timeout and the daemon's kill end every process
//!    of the user (`kill-uid`). There is no memory watch: the lease books the whole Mac.
//!    What the lease used is reported all the same ([`ResourceUsage`], so the server
//!    writes its `action usage:` line): the action's wall time, from just before the
//!    helper starts it to when it exits, times out or is killed, in its result or, for a
//!    lease that ends without one, through [`Runtime::failed_usage`]. CPU time and peak
//!    memory are not measured (the helper runs the action as the lease user, and nothing
//!    here reads that user's processes), so they are absent, not zero.
//! 3. **Restoring.** `kill-uid`, the lease directory removed, `session-idle` (auto-login
//!    cleared and the same restart, back to the login window), `user-delete`. Only then
//!    does the lease report the action's outcome.
//!
//! **Across the restarts.** Before each step the runtime writes how far it got to its
//! lease record ([`state`]), and it reports its phase (`Preparing` or `Restoring`
//! naming the lease, then `Serving`) through [`WholeMachineRuntime::phase`], which the
//! daemon sends in its `Hello`. The server keeps the lease while the node is silent
//! for the switch, and sends its `Start` again when the restarted daemon names it; the
//! runtime then takes the lease up at the step its record names, without repeating a
//! step done. At start the runtime runs the helper's boot reset, naming the recorded
//! lease, so the helper keeps that lease's session. A `kill` of a recorded lease that
//! is not running (the server no longer holds it) runs the restore.
//!
//! **A restart while the action runs** (a crash or an update, not a session switch)
//! is not taken up: the record says `started` from just before the `run`, and the
//! action's processes outlive the daemon (the helper runs them) while its result is
//! lost with it. At start the runtime kills every process of the lease user
//! (`kill-uid`) before anything else, records the lease as lost ("its result is
//! lost", [`RuntimeError::Lost`]: the farm's, not the action's) and reports
//! `Restoring`; the `Start` sent again, or the server's `Cancel`, then runs the
//! restore. The action never runs a second time on the node beside the first: the
//! server runs it again once the lost result arrives, on another node if one can.
//!
//! **A failed cleanup quarantines the node**: any restore step that fails writes the
//! quarantine file with why, the phase becomes `Quarantined`, and every later lease is
//! refused, by this daemon and by every later one on the node, until an operator
//! repairs the Mac, removes the file and restarts the daemon. The lease still reports
//! its action's outcome; the node is never released silently.
//!
//! One lease at a time: the server places a whole-machine lease only on an empty node,
//! and the runtime refuses a second while one holds its record.

mod action;
mod session;
pub mod state;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kbf_daemon::cas::Cas;
use kbf_daemon::{Runtime, RuntimeError, Work};
use kbf_outputs::OutputLimits;
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::ResourceUsage;
use kbf_types::{LeaseId, LostWhy, NodePhase};
use tokio::sync::{oneshot, watch};

pub use session::{Exit, HelperSession, Session};
use state::{Files, Record, Step};

/// The driver name in the node report.
pub const DRIVER: &str = "native-whole-machine";
/// The lease kind this runtime runs.
pub const KIND: &str = "whole_machine";

/// How long the runtime waits for the lease user's GUI session, counted from when the
/// lease was first taken: less than the scheduler's session switch grace (10 minutes),
/// so the action never starts after the server has given the lease up.
pub const SESSION_WAIT: Duration = Duration::from_secs(480);

/// A killer waiting for the lease's work to stop.
type Stop = oneshot::Sender<()>;

/// Where and how the whole-machine runtime works.
#[derive(Clone, Debug)]
pub struct WholeMachineConfig {
    /// Its lease record, its quarantine file and its lease directories.
    pub dir: PathBuf,
    /// The timeout of an action that names none.
    pub default_timeout: Duration,
    /// How much output one action may leave.
    pub outputs: OutputLimits,
    /// How long one lease's whole output upload (outputs, stdout and stderr) may take
    /// before the lease fails saying so ([`kbf_daemon::upload_within`]).
    pub upload_deadline: Duration,
    /// How long the GUI session may take to come up ([`SESSION_WAIT`]).
    pub session_wait: Duration,
    /// How often the helper is asked whether it is up.
    pub session_poll: Duration,
}

impl WholeMachineConfig {
    /// Files in `dir`, a one hour default timeout, the default output limits and upload
    /// deadline ([`kbf_daemon::UPLOAD_DEADLINE`]), and the
    /// session asked about every 2 seconds for [`SESSION_WAIT`].
    #[must_use]
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            default_timeout: Duration::from_secs(3600),
            outputs: OutputLimits::DEFAULT,
            upload_deadline: kbf_daemon::UPLOAD_DEADLINE,
            session_wait: SESSION_WAIT,
            session_poll: Duration::from_secs(2),
        }
    }
}

/// Runs `whole_machine` leases in their own users' GUI sessions (see the module
/// documentation).
pub struct WholeMachineRuntime<S, C> {
    config: WholeMachineConfig,
    session: Arc<S>,
    cas: Arc<C>,
    files: Files,
    phase: watch::Sender<NodePhase>,
    /// Held while a lease or a restore runs: one at a time.
    busy: tokio::sync::Mutex<()>,
    stops: Mutex<BTreeMap<LeaseId, oneshot::Sender<Stop>>>,
    /// What the last lease that ended without a result used, until the daemon asks
    /// ([`Runtime::failed_usage`]); one lease runs at a time, and the daemon asks as soon
    /// as a run fails, so one slot is all there is to keep.
    failed: Mutex<Option<(LeaseId, ResourceUsage)>>,
}

impl<S, C> std::fmt::Debug for WholeMachineRuntime<S, C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WholeMachineRuntime")
            .field("dir", &self.config.dir)
            .field("phase", &*self.phase.borrow())
            .finish_non_exhaustive()
    }
}

impl<S: Session, C: Cas> WholeMachineRuntime<S, C> {
    /// The runtime over `session` and `cas`, in `config.dir` (made if missing). Reads
    /// the quarantine and the lease record a previous daemon left, and runs the
    /// helper's boot reset naming that lease; a lease user the reset deleted is not
    /// cleaned up again. A failed boot reset quarantines the node. A recorded lease whose
    /// action had started has its user's processes killed and is failed, to be restored
    /// (`lost_mid_action`). All of it is done
    /// when this returns, so before the daemon says `Hello`.
    ///
    /// # Errors
    /// The directory cannot be made, or the record or quarantine cannot be read or
    /// written.
    pub fn new(config: WholeMachineConfig, session: S, cas: Arc<C>) -> std::io::Result<Self> {
        std::fs::create_dir_all(&config.dir)?;
        let files = Files::new(config.dir.clone());
        let mut record = files.lease()?;
        let mut quarantine = files.quarantine()?;
        match session.boot_reset(record.as_ref().map(|r| r.lease)) {
            Ok(deleted) => {
                if let Some(record) = &mut record
                    && deleted.contains(&record.lease)
                {
                    record.user = false;
                    files.keep(record)?;
                }
            }
            Err(why) if quarantine.is_none() => {
                let why = format!("the boot reset failed: {why}");
                files.quarantined(&why)?;
                quarantine = Some(why);
            }
            Err(why) => tracing::warn!("the boot reset failed again: {why}"),
        }
        if let Some(record) = &mut record
            && record.step == Step::Started
        {
            lost_mid_action(&session, record);
            files.keep(record)?;
        }
        let phase = match (quarantine, &record) {
            (Some(why), _) => NodePhase::Quarantined(why),
            (None, Some(r)) if r.step == Step::Preparing => NodePhase::Preparing(r.lease),
            (None, Some(r)) => NodePhase::Restoring(r.lease),
            (None, None) => NodePhase::Serving,
        };
        tracing::info!(phase = phase.name(), "whole-machine runtime ready");
        Ok(Self {
            config,
            session: Arc::new(session),
            cas,
            files,
            phase: watch::Sender::new(phase),
            busy: tokio::sync::Mutex::new(()),
            stops: Mutex::new(BTreeMap::new()),
            failed: Mutex::new(None),
        })
    }

    /// The node's phase as this runtime sees it, for the daemon's `Hello`
    /// (`kbf_daemon::Daemon::with_phase`).
    #[must_use]
    pub fn phase(&self) -> watch::Receiver<NodePhase> {
        self.phase.subscribe()
    }

    fn stops(&self) -> MutexGuard<'_, BTreeMap<LeaseId, oneshot::Sender<Stop>>> {
        lock(&self.stops)
    }

    /// Calls the helper off the async threads.
    async fn helper<T: Send + 'static>(
        &self,
        call: impl FnOnce(&S) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let session = Arc::clone(&self.session);
        tokio::task::spawn_blocking(move || call(&session))
            .await
            .unwrap_or_else(action::join_failed)
    }

    /// Keeps `usage` for [`Runtime::failed_usage`] of `lease`, in place of any earlier.
    fn keep_failed(&self, lease: LeaseId, usage: ResourceUsage) {
        *self.failed.lock().unwrap_or_else(PoisonError::into_inner) = Some((lease, usage));
    }

    /// Keeps `record`, or says why it could not.
    fn keep(&self, record: &Record) -> Result<(), String> {
        self.files
            .keep(record)
            .map_err(io_at("writing the lease record"))
    }

    /// One lease, from wherever its record says it is to its outcome.
    async fn lease(
        &self,
        work: &Work,
        stop: &mut oneshot::Receiver<Stop>,
        killer: &mut Option<Stop>,
    ) -> Result<ActionResult, RuntimeError> {
        if let NodePhase::Quarantined(why) = &*self.phase.borrow() {
            return Err(RuntimeError::Failed(format!(
                "this node is quarantined: {why}"
            )));
        }
        let id = work.lease_id;
        let found = self
            .files
            .lease()
            .map_err(io_at("reading the lease record"))
            .map_err(RuntimeError::Failed)?;
        let mut record = match found {
            Some(record) if record.lease == id => record,
            Some(record) => {
                return Err(RuntimeError::Failed(format!(
                    "lease {} still holds this node",
                    record.lease
                )));
            }
            None => {
                let record = Record::new(id, unix_ms());
                self.keep(&record).map_err(RuntimeError::Failed)?;
                record
            }
        };
        if record.step == Step::Preparing {
            self.phase.send_replace(NodePhase::Preparing(id));
            let outcome = self.prepare_and_run(work, &mut record, stop, killer).await;
            record.step = Step::Restoring;
            record.outcome = Some(outcome);
        }
        self.restore(record).await
    }

    /// The lease's user, its GUI session, and its action there.
    async fn prepare_and_run(
        &self,
        work: &Work,
        record: &mut Record,
        stop: &mut oneshot::Receiver<Stop>,
        killer: &mut Option<Stop>,
    ) -> Result<ActionResult, RuntimeError> {
        let id = record.lease;
        if !record.user {
            self.helper(move |s| s.user_create(id))
                .await
                .map_err(failed_at("user-create"))?;
            record.user = true;
            self.keep(record).map_err(RuntimeError::Failed)?;
        }
        if !record.login {
            // Kept first: the restart the login asks for may end this process.
            record.login = true;
            self.keep(record).map_err(RuntimeError::Failed)?;
            self.helper(move |s| s.session_login(id))
                .await
                .map_err(failed_at("session-login"))?;
        }
        self.wait_for_session(id, record.since_ms, stop, killer)
            .await?;
        self.run_in_session(work, record, stop, killer).await
    }

    /// Asks the helper until the lease user's GUI session is up, the session wait has
    /// passed since `since_ms`, or the lease is killed.
    async fn wait_for_session(
        &self,
        id: LeaseId,
        since_ms: u64,
        stop: &mut oneshot::Receiver<Stop>,
        killer: &mut Option<Stop>,
    ) -> Result<(), RuntimeError> {
        let wait = self.config.session_wait;
        let deadline = since_ms.saturating_add(u64::try_from(wait.as_millis()).unwrap_or(u64::MAX));
        loop {
            let ready = self
                .helper(move |s| s.session_ready(id))
                .await
                .map_err(failed_at("session-ready"))?;
            if ready {
                return Ok(());
            }
            if unix_ms() >= deadline {
                return Err(RuntimeError::Failed(format!(
                    "the GUI session of lease {id}'s user was not up within {wait:?}"
                )));
            }
            tokio::select! {
                () = tokio::time::sleep(self.config.session_poll) => {}
                Ok(by) = &mut *stop => {
                    *killer = Some(by);
                    return Err(RuntimeError::Killed);
                }
            }
        }
    }

    /// Cleans the node after `record`'s lease and returns its outcome: each step done
    /// is recorded before the next, and a step that fails quarantines the node.
    async fn restore(&self, mut record: Record) -> Result<ActionResult, RuntimeError> {
        let id = record.lease;
        self.phase.send_replace(NodePhase::Restoring(id));
        if let Err(why) = self.restore_steps(&mut record).await {
            let why = format!("the cleanup after lease {id} failed: {why}");
            tracing::error!("{why}; the node is quarantined");
            if let Err(e) = self.files.quarantined(&why) {
                tracing::error!("writing the quarantine file: {e}");
            }
            self.phase.send_replace(NodePhase::Quarantined(why));
        } else {
            self.phase.send_replace(NodePhase::Serving);
        }
        record.outcome.take().unwrap_or(Err(RuntimeError::Killed))
    }

    async fn restore_steps(&self, record: &mut Record) -> Result<(), String> {
        let id = record.lease;
        self.keep(record)?;
        if record.user && !record.killed {
            self.helper(move |s| s.kill_uid(id))
                .await
                .map_err(said_at("kill-uid"))?;
            record.killed = true;
            self.keep(record)?;
        }
        action::remove_dir(&self.lease_dir(id)).await?;
        if record.login && !record.idle {
            // Kept first: the restart may end this process.
            record.idle = true;
            self.keep(record)?;
            self.helper(S::session_idle)
                .await
                .map_err(said_at("session-idle"))?;
        }
        if record.user {
            self.helper(move |s| s.user_delete(id))
                .await
                .map_err(said_at("user-delete"))?;
        }
        self.files
            .forget()
            .map_err(io_at("removing the lease record"))
    }

    fn lease_dir(&self, id: LeaseId) -> PathBuf {
        self.config
            .dir
            .join(format!("lease-{}-{}", id.term, id.seq))
    }
}

impl<S: Session, C: Cas> Runtime for WholeMachineRuntime<S, C> {
    fn driver(&self) -> &'static str {
        DRIVER
    }

    fn serves(&self, kind: &str) -> bool {
        kind == KIND
    }

    async fn run(&self, work: Work) -> Result<ActionResult, RuntimeError> {
        let (stop_tx, mut stop) = oneshot::channel();
        let registered = Registered::new(&self.stops, work.lease_id, stop_tx);
        let mut killer = None;
        let outcome = {
            let _busy = self.busy.lock().await;
            self.lease(&work, &mut stop, &mut killer).await
        };
        drop(registered);
        // A kill that came after the work ended still waits for the restore.
        if let Some(by) = killer.or_else(|| stop.try_recv().ok()) {
            let _ = by.send(());
        }
        outcome
    }

    fn failed_usage(&self, lease_id: LeaseId) -> Option<ResourceUsage> {
        let mut failed = self.failed.lock().unwrap_or_else(PoisonError::into_inner);
        failed
            .take_if(|(lease, _)| *lease == lease_id)
            .map(|(_, usage)| usage)
    }

    /// Stops a running lease and waits for its restore. A lease that is not running
    /// but holds the lease record (the server gave it up while the daemon restarted)
    /// is restored now, its outcome `Killed`; any other is left alone.
    async fn kill(&self, lease_id: LeaseId) {
        let running = self.stops().remove(&lease_id);
        if let Some(stop) = running {
            let (tx, rx) = oneshot::channel();
            // The run answers once its restore is done; if it ended without seeing
            // the kill, `tx` is dropped with it.
            let _ = stop.send(tx);
            let _ = rx.await;
            return;
        }
        let _busy = self.busy.lock().await;
        if matches!(&*self.phase.borrow(), NodePhase::Quarantined(_)) {
            return;
        }
        let Ok(Some(mut record)) = self.files.lease() else {
            return;
        };
        if record.lease != lease_id {
            return;
        }
        tracing::warn!(lease = %lease_id, "the server no longer holds the recorded lease: restoring");
        record.step = Step::Restoring;
        record.outcome.get_or_insert(Err(RuntimeError::Killed));
        let _ = self.restore(record).await;
    }
}

/// A lease's entry in the kill table, removed when its run ends or is dropped (the
/// daemon dying mid-run drops it).
struct Registered<'a> {
    stops: &'a Mutex<BTreeMap<LeaseId, oneshot::Sender<Stop>>>,
    id: LeaseId,
}

impl<'a> Registered<'a> {
    fn new(
        stops: &'a Mutex<BTreeMap<LeaseId, oneshot::Sender<Stop>>>,
        id: LeaseId,
        stop: oneshot::Sender<Stop>,
    ) -> Self {
        lock(stops).insert(id, stop);
        Self { stops, id }
    }
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        lock(self.stops).remove(&self.id);
    }
}

fn lock(
    stops: &Mutex<BTreeMap<LeaseId, oneshot::Sender<Stop>>>,
) -> MutexGuard<'_, BTreeMap<LeaseId, oneshot::Sender<Stop>>> {
    // Each update is one map operation.
    stops.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Takes up `record`'s lease after a daemon that died while its action ran (a crash or
/// an update, not a session switch): the action's processes may still run, and its
/// result is lost. Kills every process of the lease user first, then turns the lease
/// to its restore, lost (the server runs the action again once the node is restored,
/// as a farm fault), so the action is never run a second time beside the first.
/// A `kill-uid` that fails here is retried as the restore's first step, whose failure
/// quarantines the node.
fn lost_mid_action<S: Session>(session: &S, record: &mut Record) {
    let id = record.lease;
    tracing::warn!(lease = %id, "the daemon restarted while the action ran: killing it");
    if record.user {
        match session.kill_uid(id) {
            Ok(()) => record.killed = true,
            Err(why) => tracing::warn!("kill-uid at start failed: {why}"),
        }
    }
    record.step = Step::Restoring;
    record.outcome = Some(Err(RuntimeError::Lost(
        LostWhy::DaemonRestarted,
        format!("the daemon restarted while lease {id}'s action ran: its result is lost"),
    )));
}

/// The lease's failure at step `what`, from the helper's (or the file's) reason.
pub(crate) fn failed_at(what: &'static str) -> impl Fn(String) -> RuntimeError {
    move |why| RuntimeError::Failed(said_at(what)(why))
}

/// `what: why`, for an I/O error.
pub(crate) fn io_at(what: impl std::fmt::Display) -> impl Fn(std::io::Error) -> String {
    move |why| format!("{what}: {why}")
}

/// `what: why`.
fn said_at(what: &'static str) -> impl Fn(String) -> String {
    move |why| format!("{what}: {why}")
}

/// The wall clock in milliseconds since the Unix epoch: the session wait spans the
/// daemon's restarts, which no monotonic clock does.
fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}
