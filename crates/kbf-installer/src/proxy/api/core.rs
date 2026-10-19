//! The state of the update API: everything that decides, with no task and no clock of its
//! own. The [`actor`](super::actor) owns one `Core` and feeds it one command at a time, so
//! the checks and the changes of one call are never interleaved with another's.
//!
//! One update lives in one of four places: nowhere, [`Active::Staging`] (the order is
//! written and the installer is fetching, verifying and staging; the API owns these
//! phases and no child exists), [`Active::Ended`] (it ended there: refused, found
//! incompatible, aborted, or not staged by its [`StagingDeadline`]), or with the
//! [`Supervisor`] (it was handed over once the staged release passed the gate; the
//! supervisor owns every later phase).
//!
//! A staging update is also in the request map, with the deadline it must stage by
//! ([`Waiting`]). A restarted `Core` takes it up again with that deadline.

use std::sync::Arc;

use hyper::StatusCode;

use super::idempotency::{Entry, Idempotency, Waiting};
use super::model::{
    RollbackBlocked, StatusBody, UpdateId, UpdateRequest, abortable, rollback_gate,
};
use super::refusal;
use super::traits::{
    Clock, Compat, CompatGate, InstallerState, InstallerStatus, OrderWriter, RestartMarker,
    RunningUpdate, StatusReader, Supervisor, SupervisorError, UpdateSpec,
};
use crate::CommitId;
use crate::proxy::model::{Failure, Locator, Phase, StagedFacts, Step, Verb};

/// The longest `request_id` and the longest force-retire reason accepted.
const MAX_REQUEST_ID: usize = 128;
const MAX_REASON: usize = 512;

/// A named place where a test makes the process die.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashPoint {
    /// The request id is on disk and nothing has been done or answered.
    AfterRequestSaved,
}

/// The process died: the call ends without an answer and the actor stops.
#[derive(Debug, PartialEq, Eq)]
pub struct Crash;

pub trait CrashHook: Send {
    fn at(&mut self, point: CrashPoint) -> Result<(), Crash>;
}

/// The hook of a process that does not die on purpose.
pub struct NoCrash;

impl CrashHook for NoCrash {
    fn at(&mut self, _point: CrashPoint) -> Result<(), Crash> {
        Ok(())
    }
}

/// A refusal with the status and the body the client gets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    /// One more field of the body, such as `("rule", "R1")`.
    pub extra: Option<(&'static str, String)>,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError {
            status,
            code,
            message: message.into(),
            extra: None,
        }
    }

    fn conflict(code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::CONFLICT, code, message)
    }

    fn bad(code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::BAD_REQUEST, code, message)
    }

    fn unavailable(code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, code, message)
    }

    fn with(mut self, key: &'static str, value: impl Into<String>) -> ApiError {
        self.extra = Some((key, value.into()));
        self
    }
}

impl From<SupervisorError> for ApiError {
    fn from(e: SupervisorError) -> ApiError {
        match e {
            SupervisorError::NotOpen => ApiError::conflict("no_update_open", "no update is open"),
            SupervisorError::Refused(why) => ApiError::conflict("refused", why),
            SupervisorError::Failed(why) => {
                ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "supervisor_failed", why)
            }
        }
    }
}

/// A call ended badly: with an answer for the client, or with the death of the process.
#[derive(Debug, PartialEq, Eq)]
pub enum Fault {
    Api(ApiError),
    Crash,
}

impl From<ApiError> for Fault {
    fn from(e: ApiError) -> Fault {
        Fault::Api(e)
    }
}

impl From<Crash> for Fault {
    fn from(_: Crash) -> Fault {
        Fault::Crash
    }
}

/// What a successful call answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    /// `{update_id, phase}`: the answer of `POST`, `abort` and `rollback`.
    Update {
        update_id: UpdateId,
        phase: Phase,
    },
    Status(Box<StatusBody>),
    Retired {
        update_id: Option<UpdateId>,
        dropped_streams: u64,
    },
}

/// How long an accepted update may wait for the installer to stage its release.
///
/// It counts from the moment the order is written, whatever the installer reports in
/// between (a fetch under way does not move it). When it has run out and the release is
/// not staged, the update ends `failed` at step `stage` with reason `timeout`, and a
/// `revert` order drops what the installer may still stage.
///
/// It is below [`kbf_update::MAX_AGE_SECS`], the age at which the installer drops an
/// order it has not acted on, so an update never waits on an order that is already gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StagingDeadline(u64);

/// A staging deadline outside `1..=StagingDeadline::MAX_S`.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "a staging deadline is 1 to {} seconds, below the {} seconds an order lives in the spool; got {}",
    StagingDeadline::MAX_S,
    kbf_update::MAX_AGE_SECS,
    .0
)]
pub struct DeadlineRange(pub u64);

impl StagingDeadline {
    /// Fifteen minutes.
    pub const DEFAULT: StagingDeadline = StagingDeadline(900);
    /// The longest deadline: one second less than the age an order is dropped at.
    pub const MAX_S: u64 = kbf_update::MAX_AGE_SECS - 1;

    pub fn from_secs(secs: u64) -> Result<StagingDeadline, DeadlineRange> {
        if (1..=Self::MAX_S).contains(&secs) {
            Ok(StagingDeadline(secs))
        } else {
            Err(DeadlineRange(secs))
        }
    }

    pub fn secs(self) -> u64 {
        self.0
    }
}

/// What the API is told about its host.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// The confirm window the proxy asks for in its orders (clamped by the installer).
    pub window_s: u32,
    /// How long an accepted update waits for its release to be staged.
    pub staging_deadline: StagingDeadline,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            window_s: 900,
            staging_deadline: StagingDeadline::DEFAULT,
        }
    }
}

/// The `failure.reason` of an update that was not staged in time.
const STAGING_TIMEOUT: &str = "timeout";

/// Everything a `Core` talks to.
pub struct Parts {
    pub supervisor: Box<dyn Supervisor>,
    pub orders: Box<dyn OrderWriter>,
    pub status: Box<dyn StatusReader>,
    pub compat: Box<dyn CompatGate>,
    pub clock: Arc<dyn Clock>,
    pub marker: RestartMarker,
    pub requests: Idempotency,
    pub hook: Box<dyn CrashHook>,
    pub config: Config,
}

struct Staging {
    update_id: UpdateId,
    request_id: String,
    from: Option<CommitId>,
    to: CommitId,
    nodes: bool,
    rollback: bool,
    order: Locator,
    phase: Phase,
    since: u64,
    /// Unix seconds by which the release must be staged; fixed when the order is written.
    deadline_s: u64,
}

struct Ended {
    update_id: UpdateId,
    from: Option<CommitId>,
    to: CommitId,
    phase: Phase,
    since: u64,
    failure: Option<Failure>,
}

enum Active {
    None,
    Staging(Box<Staging>),
    Ended(Box<Ended>),
    Supervised,
}

pub struct Core {
    parts: Parts,
    active: Active,
}

impl Core {
    pub fn new(parts: Parts) -> Core {
        let mut core = Core {
            parts,
            active: Active::None,
        };
        core.resume();
        core
    }

    /// A process that starts with an update still waiting for the installer (its
    /// predecessor wrote the order and died before the release was staged) goes on
    /// waiting for it, with the deadline the predecessor set. An update the supervisor
    /// already has was handed over before the death: it is no longer the API's.
    ///
    /// When the predecessor ended the update but could not save the end of the wait, the
    /// entry still says waiting: the update is taken up, found past its deadline and
    /// reverted a second time, for the same commit. The design has the root half check that
    /// the commit a `revert` names is the staged one, so a second order finds nothing to
    /// drop; the update ends `failed` as before.
    fn resume(&mut self) {
        let Some((entry, waiting)) = self.parts.requests.waiting() else {
            return;
        };
        let (entry, waiting) = (entry.clone(), waiting.clone());
        let handed_over = self
            .parts
            .supervisor
            .status()
            .update
            .is_some_and(|u| u.update_id == entry.update_id);
        if handed_over {
            self.stop_waiting(&entry.request_id);
            return;
        }
        // No deadline is further off than the deadline is long: one that is (the clock
        // went back between the two processes) counts from now.
        let latest = self
            .parts
            .clock
            .wall_s()
            .saturating_add(self.parts.config.staging_deadline.secs());
        self.active = Active::Staging(Box::new(Staging {
            update_id: entry.update_id,
            request_id: entry.request_id,
            from: waiting.from,
            to: entry.commit,
            nodes: entry.nodes,
            rollback: entry.rollback,
            order: waiting.order,
            phase: Phase::Accepted,
            since: waiting.since_s,
            deadline_s: waiting.deadline_s.min(latest),
        }));
    }

    #[cfg(test)]
    pub(super) fn set_hook(&mut self, hook: Box<dyn CrashHook>) {
        self.parts.hook = hook;
    }

    #[cfg(test)]
    pub(super) fn set_config(&mut self, config: Config) {
        self.parts.config = config;
    }

    /// `POST /v1/update/frontend`.
    pub fn post(&mut self, req: UpdateRequest) -> Result<Reply, Fault> {
        valid_request_id(&req.request_id)?;
        if self.parts.requests.lost() {
            return Err(ApiError::conflict(
                "state_lost",
                "the request map is unreadable; no new update is accepted until it is repaired",
            )
            .into());
        }
        self.advance();
        if let Some(entry) = self.parts.requests.get(&req.request_id).cloned() {
            return self.replay(&entry, &req);
        }
        let status = self.preflight(&req)?;
        let entry = self
            .parts
            .requests
            .insert(&req.request_id, &req.commit, req.nodes, req.rollback)
            .map_err(|e| ApiError::unavailable("state_unwritable", e.to_string()))?;
        self.parts.hook.at(CrashPoint::AfterRequestSaved)?;
        self.launch(&entry, status.installed_commit)
    }

    /// A request id seen before: the same update, whatever it has become.
    fn replay(&mut self, entry: &Entry, req: &UpdateRequest) -> Result<Reply, Fault> {
        if (&entry.commit, entry.nodes, entry.rollback) != (&req.commit, req.nodes, req.rollback) {
            return Err(ApiError::conflict(
                "request_id_reused",
                "this request_id was used for a different request",
            )
            .into());
        }
        if let Some(phase) = self.phase_of(&entry.update_id) {
            return Ok(Reply::Update {
                update_id: entry.update_id.clone(),
                phase,
            });
        }
        if entry.started || self.is_open() {
            return Err(ApiError::conflict(
                "request_id_retired",
                "the update of this request_id is no longer tracked",
            )
            .into());
        }
        // Accepted, and the process died (or the order failed) before it began.
        let status = self.preflight(req)?;
        self.launch(entry, status.installed_commit)
    }

    /// The checks every new update passes before anything is saved or written.
    fn preflight(&mut self, req: &UpdateRequest) -> Result<InstallerStatus, ApiError> {
        if self.is_open() {
            return Err(ApiError::conflict(
                "update_open",
                "another update is open; abort it or wait",
            ));
        }
        let sup = self.parts.supervisor.status();
        if sup.old_child_draining {
            return Err(ApiError::conflict(
                "old_child_draining",
                "an old child is still draining; wait or force-retire it",
            ));
        }
        let status = self
            .parts
            .status
            .read()
            .map_err(|e| ApiError::unavailable("status_unreadable", e.to_string()))?;
        let running = sup.running.as_ref().or(status.installed_commit.as_ref());
        if running == Some(&req.commit) {
            return Err(ApiError::conflict(
                "same_commit",
                "this commit is already running",
            ));
        }
        match self.parts.compat.check(&req.commit, None) {
            Compat::Compatible => {}
            Compat::Incompatible { rule } => {
                return Err(ApiError::conflict(
                    "incompatible",
                    format!("the release breaks compatibility rule {rule}"),
                )
                .with("rule", rule));
            }
            Compat::Incomplete { reason } => {
                return Err(ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "incomplete_release",
                    reason,
                ));
            }
        }
        if !matches!(
            status.state,
            InstallerState::Idle
                | InstallerState::Confirmed
                | InstallerState::Reverted
                | InstallerState::Refused
        ) {
            return Err(ApiError::conflict(
                "installer_busy",
                "the installer has a fetch, a staged release or a swap in progress",
            ));
        }
        Ok(status)
    }

    /// Writes the order and starts tracking the update.
    fn launch(&mut self, entry: &Entry, from: Option<CommitId>) -> Result<Reply, Fault> {
        let order = self
            .parts
            .orders
            .write(
                Verb::FrontendUpdate,
                &entry.commit,
                self.parts.config.window_s,
            )
            .map_err(|e| ApiError::unavailable("order_unwritable", e.to_string()))?;
        let since = self.parts.clock.wall_s();
        let deadline_s = since.saturating_add(self.parts.config.staging_deadline.secs());
        let waiting = Waiting {
            from: from.clone(),
            order: order.clone(),
            since_s: since,
            deadline_s,
        };
        self.active = Active::Staging(Box::new(Staging {
            update_id: entry.update_id.clone(),
            request_id: entry.request_id.clone(),
            from,
            to: entry.commit.clone(),
            nodes: entry.nodes,
            rollback: entry.rollback,
            order,
            phase: Phase::Accepted,
            since,
            deadline_s,
        }));
        if let Err(e) = self.parts.requests.mark_started(&entry.request_id, waiting) {
            // The update is in motion; a retry finds it by its phase, but a restart will not.
            tracing::warn!(error = %e, "the request map could not record the start");
        }
        Ok(Reply::Update {
            update_id: entry.update_id.clone(),
            phase: Phase::Accepted,
        })
    }

    /// An update is open: staging, or open with the supervisor.
    fn is_open(&self) -> bool {
        matches!(self.active, Active::Staging(_))
            || self
                .parts
                .supervisor
                .status()
                .update
                .is_some_and(|u| !u.phase.is_terminal())
    }

    fn phase_of(&self, id: &UpdateId) -> Option<Phase> {
        match &self.active {
            Active::Staging(s) if &s.update_id == id => return Some(s.phase),
            Active::Ended(e) if &e.update_id == id => return Some(e.phase),
            _ => {}
        }
        self.parts
            .supervisor
            .status()
            .update
            .filter(|u| &u.update_id == id)
            .map(|u| u.phase)
    }

    /// Moves a staging update along by what `status.json` says now. An unreadable file
    /// changes nothing until the deadline, which does not wait for the file: a missing or
    /// torn `status.json` is one of the ways an update is never staged.
    pub fn advance(&mut self) {
        let Some(mut st) = self.take_staging() else {
            return;
        };
        let status = match self.parts.status.read() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "status.json is unreadable; the update waits");
                if self.parts.clock.wall_s() >= st.deadline_s {
                    self.time_out(st);
                } else {
                    self.active = Active::Staging(st);
                }
                return;
            }
        };
        if status.state == InstallerState::Refused
            && status.refused_order_id == Some(st.order.order_id)
        {
            let word = status.refusal_word().to_owned();
            let failure = Failure {
                step: refusal::step(&word),
                reason: word,
            };
            self.active = self.end(&st, Phase::Failed, Some(failure));
            return;
        }
        let facts = status.staged_facts().filter(|f| f.commit == st.to);
        match facts {
            // A release found staged is used, however late: the work is done.
            Some(facts) => self.active = self.hand_over(st, &facts),
            None if self.parts.clock.wall_s() >= st.deadline_s => self.time_out(st),
            None => {
                let phase = if status.state == InstallerState::Fetching {
                    Phase::Fetching
                } else {
                    st.phase
                };
                if phase != st.phase {
                    st.phase = phase;
                    st.since = self.parts.clock.wall_s();
                }
                self.active = Active::Staging(st);
            }
        }
    }

    /// The staging update, taken out of `active` (which is `None` until it is put back);
    /// any other state stays where it is.
    fn take_staging(&mut self) -> Option<Box<Staging>> {
        match std::mem::replace(&mut self.active, Active::None) {
            Active::Staging(st) => Some(st),
            other => {
                self.active = other;
                None
            }
        }
    }

    /// The release is staged: judge it, then hand it to the supervisor.
    fn hand_over(&mut self, st: Box<Staging>, facts: &StagedFacts) -> Active {
        let failure = match self.parts.compat.check(&st.to, Some(facts)) {
            Compat::Compatible => {
                let spec = UpdateSpec {
                    update_id: st.update_id.clone(),
                    request_id: st.request_id.clone(),
                    from: st.from.clone(),
                    to: st.to.clone(),
                    nodes: st.nodes,
                    rollback: st.rollback,
                    facts: facts.clone(),
                    order: st.order.clone(),
                };
                match self.parts.supervisor.start(&spec) {
                    Ok(()) => {
                        self.stop_waiting(&st.request_id);
                        return Active::Supervised;
                    }
                    Err(e) => Failure {
                        step: Step::Start,
                        reason: e.to_string(),
                    },
                }
            }
            Compat::Incompatible { rule } => Failure {
                step: Step::Compat,
                reason: rule,
            },
            Compat::Incomplete { reason } => Failure {
                step: Step::Compat,
                reason,
            },
        };
        self.drop_staged(&st);
        self.end(&st, Phase::Failed, Some(failure))
    }

    /// Asks the installer to drop the staged release of an update that will not go on.
    fn drop_staged(&mut self, st: &Staging) {
        if let Err(e) = self
            .parts
            .orders
            .write(Verb::Revert, &st.to, self.parts.config.window_s)
        {
            tracing::warn!(error = %e, "the revert order could not be written");
        }
    }

    /// The installer did not stage the release by the deadline. A `revert` order drops
    /// what it may still stage (an order it has not read, a fetch under way), so that the
    /// next update is not refused for a staged release nobody tracks. When that order
    /// cannot be written the update stays open and the next look tries again: ending it
    /// would leave a staged release no call of this API can drop.
    fn time_out(&mut self, st: Box<Staging>) {
        let window_s = self.parts.config.window_s;
        if let Err(e) = self.parts.orders.write(Verb::Revert, &st.to, window_s) {
            tracing::warn!(error = %e, "the revert for a staging timeout could not be written");
            self.active = Active::Staging(st);
            return;
        }
        let failure = Failure {
            step: Step::Stage,
            reason: STAGING_TIMEOUT.to_owned(),
        };
        self.active = self.end(&st, Phase::Failed, Some(failure));
    }

    /// The update no longer waits for the installer (it was handed over or it ended): a
    /// restarted process must not take it up again.
    fn stop_waiting(&mut self, request_id: &str) {
        if let Err(e) = self.parts.requests.finish_waiting(request_id) {
            tracing::warn!(error = %e, "the request map could not record the end of the wait");
        }
    }

    fn end(&mut self, st: &Staging, phase: Phase, failure: Option<Failure>) -> Active {
        self.stop_waiting(&st.request_id);
        Active::Ended(Box::new(Ended {
            update_id: st.update_id.clone(),
            from: st.from.clone(),
            to: st.to.clone(),
            phase,
            since: self.parts.clock.wall_s(),
            failure,
        }))
    }

    /// `GET /v1/update/frontend`.
    pub fn status(&mut self) -> Reply {
        self.advance();
        let restart_pending = self.parts.marker.is_set();
        let mut body = StatusBody {
            update_id: None,
            from: None,
            to: None,
            phase: None,
            since: None,
            active_slot: None,
            restart_pending,
            rollback_allowed: false,
            rollback_blocked_reason: Some(RollbackBlocked::NoUpdate),
            old: None,
            new: None,
            failure: None,
        };
        match &self.active {
            Active::Staging(s) => {
                body.update_id = Some(s.update_id.clone());
                body.from = s.from.clone();
                body.to = Some(s.to.clone());
                body.phase = Some(s.phase);
                body.since = Some(s.since);
                body.rollback_blocked_reason = Some(RollbackBlocked::NotSwitched);
            }
            Active::Ended(e) => {
                body.update_id = Some(e.update_id.clone());
                body.from = e.from.clone();
                body.to = Some(e.to.clone());
                body.phase = Some(e.phase);
                body.since = Some(e.since);
                body.failure = e.failure.clone();
            }
            _ => {
                if let Some(u) = self.parts.supervisor.status().update {
                    fill_from_supervisor(&mut body, u);
                }
            }
        }
        Reply::Status(Box::new(body))
    }

    /// `POST .../abort`: before the flip.
    pub fn abort(&mut self) -> Result<Reply, Fault> {
        self.advance();
        if let Some(st) = self.take_staging() {
            if let Err(e) =
                self.parts
                    .orders
                    .write(Verb::Revert, &st.to, self.parts.config.window_s)
            {
                self.active = Active::Staging(st);
                return Err(ApiError::unavailable("order_unwritable", e.to_string()).into());
            }
            let id = st.update_id.clone();
            self.active = self.end(&st, Phase::RolledBack, None);
            return Ok(Reply::Update {
                update_id: id,
                phase: Phase::RolledBack,
            });
        }
        let update = self.open_update()?;
        if !abortable(update.phase) {
            return Err(ApiError::conflict(
                "not_abortable",
                format!(
                    "the update is past the point of abort (phase {:?})",
                    update.phase
                ),
            )
            .into());
        }
        let phase = self.parts.supervisor.abort().map_err(ApiError::from)?;
        Ok(Reply::Update {
            update_id: update.update_id,
            phase,
        })
    }

    /// `POST .../rollback`: after the flip, while the old child lives and no node was
    /// updated.
    pub fn rollback(&mut self) -> Result<Reply, Fault> {
        self.advance();
        if matches!(self.active, Active::Staging(_)) {
            return Err(refused_rollback(RollbackBlocked::NotSwitched).into());
        }
        let update = self.open_update()?;
        rollback_gate(update.phase, update.old.is_some(), update.nodes_updated)
            .map_err(refused_rollback)?;
        let phase = self.parts.supervisor.rollback().map_err(ApiError::from)?;
        Ok(Reply::Update {
            update_id: update.update_id,
            phase,
        })
    }

    /// `POST .../force-retire`: end the old child now, dropping what it still serves.
    pub fn force_retire(&mut self, reason: &str) -> Result<Reply, Fault> {
        let reason = reason.trim();
        if reason.is_empty() {
            return Err(ApiError::bad("reason_required", "force-retire needs a reason").into());
        }
        if reason.len() > MAX_REASON {
            return Err(ApiError::bad("reason_too_long", "the reason is too long").into());
        }
        let sup = self.parts.supervisor.status();
        let has_old =
            sup.old_child_draining || sup.update.as_ref().is_some_and(|u| u.old.is_some());
        if !has_old {
            return Err(
                ApiError::conflict("no_old_child", "there is no old child to retire").into(),
            );
        }
        let retired = self
            .parts
            .supervisor
            .force_retire(reason)
            .map_err(ApiError::from)?;
        let dropped = retired.dropped_streams;
        tracing::warn!(reason, dropped, "old child force-retired");
        Ok(Reply::Retired {
            update_id: sup.update.map(|u| u.update_id),
            dropped_streams: dropped,
        })
    }

    /// The supervisor's update, if it is open.
    fn open_update(&self) -> Result<RunningUpdate, ApiError> {
        self.parts
            .supervisor
            .status()
            .update
            .filter(|u| !u.phase.is_terminal())
            .ok_or_else(|| ApiError::conflict("no_update_open", "no update is open"))
    }
}

fn refused_rollback(why: RollbackBlocked) -> ApiError {
    ApiError::conflict("rollback_refused", why.describe())
        .with("reason", format!("{why:?}").to_ascii_lowercase())
}

fn fill_from_supervisor(body: &mut StatusBody, u: RunningUpdate) {
    let gate = rollback_gate(u.phase, u.old.is_some(), u.nodes_updated);
    body.update_id = Some(u.update_id);
    body.from = Some(u.from);
    body.to = Some(u.to);
    body.phase = Some(u.phase);
    body.since = Some(u.since_s);
    body.active_slot = Some(u.active_slot);
    body.rollback_allowed = gate.is_ok();
    body.rollback_blocked_reason = gate.err();
    body.old = u.old;
    body.new = Some(u.new);
    body.failure = u.failure;
}

fn valid_request_id(id: &str) -> Result<(), ApiError> {
    let ok = !id.is_empty()
        && id.len() <= MAX_REQUEST_ID
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'));
    if ok {
        Ok(())
    } else {
        Err(ApiError::bad(
            "bad_request_id",
            "request_id is 1 to 128 characters of letters, digits and - _ . :",
        ))
    }
}
