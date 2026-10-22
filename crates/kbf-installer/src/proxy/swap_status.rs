//! `GET /v1/proxy/swap`: the state of the swap, for whatever updates the host (the package
//! manager's caller) and must know when an update is over. It is read-only, and answered by the
//! update route of a proxy that keeps its own copies of `kbf` (`--kbf-binary`), on the proxy's
//! one public port and behind the same operator token as the update API was; every other call
//! on that route is still 410 ([`Gone`]). Without `--kbf-binary` the route is the update API's,
//! which does not know the path and answers 404.
//!
//! # The answer
//!
//! `200`, `application/json`, these keys in this order:
//!
//! | key | value |
//! |---|---|
//! | `proxy_boot` | an id fixed for this proxy process: its start time as RFC 3339 UTC, a dash and the pid ([`boot_id`]) |
//! | `installed_commit` | `installed_commit` of `releases.json`, or `null` when none is recorded or the file cannot be read |
//! | `active_slot` | `"a"` or `"b"`: the slot whose child serves; `null` before the proxy is bound and while the start-up hold is in force |
//! | `active_commit` | the commit that child runs, by the swap's record of the slot (never the kbf on disk), else `null` |
//! | `update` | `null`, or the newest update, below |
//! | `pending_binary` | the commit of a changed kbf that holds still and has no swap started for it (busy, backing off after a failure), else `null` |
//!
//! `update` is the newest update this proxy knows, open or ended: `id`, `seq` (the number this
//! process gave it, [`WatchStatus::seq_of`]: one more for each update it starts or resumes),
//! `from` (`null` when unknown), `to`, `phase` (the swap's phase word as `update.json` records
//! it), `state`, `reason` (`null` unless something went wrong), `started_at` and `ended_at`
//! (RFC 3339 UTC, `null` until it ends). It is the watch's refusal while the kbf that was
//! refused is still at the path, else the swap's record. A refusal is one update for all the
//! retries of its kbf: `id`, `seq`, `started_at` and `ended_at` are those of the first failure,
//! and `reason` is that of the newest.
//!
//! # State
//!
//! | `state` | the swap's record |
//! |---|---|
//! | `running` | any phase before `done`, `draining` included: the new child serves, the staged release is confirmed and the old child is drained, and nothing is over until the swap says so |
//! | `done` | phase `done`, which is recorded after the confirm and after the old child drained |
//! | `abandoned` | phase `rolled_back`; phase `failed` at any step but those below: the swap began and the old release is serving again (a takeover over the window, a candidate that did not come up, an abort), or the new one serves and was not confirmed (`confirm`) |
//! | `refused` | phase `failed` at a step before the swap starts a candidate (`verify`, `fetch`, `order`, `host`, `compat`, `stage`, `migrate`; a failure at `start` itself is `abandoned`, even for a candidate that could not be spawned); and a refusal of the watch (the copy, the stage or the start of the swap for the kbf at the path failed), whose phase is `accepted` |
//!
//! `reason` is the failure as `<step>: <reason>`, `rolled back by the operator` for a rollback,
//! and the failed pass's text for a refusal. A kbf that cannot be run names no commit, so its
//! failure has no update to attach to and is only in the log.
//!
//! # Healthy
//!
//! The host's updater calls an install healthy when `pending_binary` is `null`, `update.state`
//! is `done`, `update.to` is the target, `active_commit` and `installed_commit` are the target
//! too, and `update.started_at` is not earlier than the install. An answer that is not `200` is
//! not healthy yet, not a failure of the swap: a proxy of the build before this one forwards the
//! path to the child's operator API, which does not serve it, and a proxy without
//! `--kbf-binary` answers 404. A swap
//! that an older build recorded has no start time, and its `started_at` is the time of its
//! newest phase, which for a `done` update is its end.

use std::sync::Arc;

use async_trait::async_trait;
use hyper::header::{ALLOW, HeaderValue};
use serde::Serialize;

use super::assemble::{Answer, Answered, Call, Gone};
use super::binary_watch::{Refusal, WatchStatus};
use super::handover_swap::Handover;
use super::model::{Failure, Intent, Phase, Slot, Step};
use super::ports::{Clock, RootOrders};
use super::releases::LocalOrders;
use super::supervisor_swap::Snapshot;
use crate::CommitId;
use crate::utc_stamp::rfc3339;

/// The path of the swap's state.
pub const PATH: &str = "/v1/proxy/swap";

/// The body of the 405 a call other than `GET` gets.
const NOT_ALLOWED: &str = r#"{"error":"only GET is allowed on the swap's state"}"#;

/// The steps at which an update that failed was refused: the ones that come before the swap
/// starts a candidate. `Start` is not among them: it is the swap having begun (it also covers
/// a candidate that could not be spawned), so a failure there is abandoned.
const REFUSED_AT: [Step; 7] = [
    Step::Verify,
    Step::Fetch,
    Step::Order,
    Step::Host,
    Step::Compat,
    Step::Stage,
    Step::Migrate,
];

/// Where the endpoint reads the swap from: the [`Handover`] on a host, a fake in a test. A read
/// may block (the swap asks the old child how many streams it has open), so it is made off the
/// runtime.
pub trait Source: Send + Sync {
    fn snapshot(&self) -> Snapshot;
}

impl Source for Handover {
    fn snapshot(&self) -> Snapshot {
        Handover::snapshot(self)
    }
}

/// How an update stands, for the one who waits on it. See the module documentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum State {
    Running,
    Done,
    Abandoned,
    Refused,
}

impl State {
    /// The state of an update the swap recorded at `phase`, which failed (if it did) at `step`.
    fn of(phase: Phase, step: Option<Step>) -> State {
        let refused = step.is_some_and(|step| REFUSED_AT.contains(&step));
        match phase {
            Phase::Done => State::Done,
            Phase::RolledBack => State::Abandoned,
            Phase::Failed if refused => State::Refused,
            Phase::Failed => State::Abandoned,
            _ => State::Running,
        }
    }
}

/// The newest update, as the endpoint shows it.
#[derive(Serialize)]
struct UpdateBody {
    id: String,
    seq: u64,
    from: Option<CommitId>,
    to: CommitId,
    phase: Phase,
    state: State,
    reason: Option<String>,
    started_at: String,
    ended_at: Option<String>,
}

/// The answer, in the order of the module documentation.
#[derive(Serialize)]
struct Body {
    proxy_boot: String,
    installed_commit: Option<CommitId>,
    active_slot: Option<Slot>,
    active_commit: Option<CommitId>,
    update: Option<UpdateBody>,
    pending_binary: Option<CommitId>,
}

/// A failure as `<step>: <reason>`.
fn reason_of(failure: &Failure) -> String {
    // The step's name as `update.json` writes it, whatever the variant's spelling.
    let named = serde_json::to_string(&failure.step).unwrap_or_default();
    let step = named.trim_matches('"');
    let why = &failure.reason;
    format!("{step}: {why}")
}

/// The swap's record of an update as the endpoint shows it, numbered `seq`. An update that an
/// older build recorded has no start time, and the time of its newest phase stands for it.
fn recorded(intent: &Intent, seq: u64) -> UpdateBody {
    let step = intent.failure.as_ref().map(|failure| failure.step);
    let started = if intent.started_at == 0 {
        intent.phase_started
    } else {
        intent.started_at
    };
    let reason = match intent.phase {
        Phase::RolledBack => Some("rolled back by the operator".to_owned()),
        _ => intent.failure.as_ref().map(reason_of),
    };
    let ended = intent.phase.is_terminal();
    UpdateBody {
        id: intent.update_id.clone(),
        seq,
        from: Some(intent.from.clone()),
        to: intent.to.clone(),
        phase: intent.phase,
        state: State::of(intent.phase, step),
        reason,
        started_at: rfc3339(started),
        ended_at: ended.then(|| rfc3339(intent.phase_started)),
    }
}

/// The watch's refusal as the endpoint shows it: an update that got no further than asked.
fn refused(refusal: &Refusal) -> UpdateBody {
    UpdateBody {
        id: refusal.id.clone(),
        seq: refusal.seq,
        from: refusal.from.clone(),
        to: refusal.to.clone(),
        phase: Phase::Accepted,
        state: State::Refused,
        reason: Some(refusal.reason.clone()),
        started_at: rfc3339(refusal.at),
        ended_at: Some(rfc3339(refusal.at)),
    }
}

/// The id of this proxy process: its start time as RFC 3339 UTC, a dash and its pid.
#[must_use]
pub fn boot_id(clock: &dyn Clock, pid: u32) -> String {
    let started = rfc3339(clock.now().as_secs());
    format!("{started}-{pid}")
}

/// The update route of a proxy that keeps its own copies: [`PATH`] answers the swap's state and
/// any other path is [`Gone`].
pub struct SwapStatus {
    source: Arc<dyn Source>,
    orders: Arc<LocalOrders>,
    watch: WatchStatus,
    boot: String,
}

impl SwapStatus {
    /// The route over `source`, reading `installed_commit` from `orders` and what waits from
    /// `watch`; `boot` is the proxy's [`boot_id`].
    #[must_use]
    pub fn new(
        source: Arc<dyn Source>,
        orders: Arc<LocalOrders>,
        watch: WatchStatus,
        boot: String,
    ) -> SwapStatus {
        SwapStatus {
            source,
            orders,
            watch,
            boot,
        }
    }

    /// The answer for `snapshot`, with the `installed_commit` that was read with it.
    fn body(&self, snapshot: Snapshot, installed: Option<CommitId>) -> Body {
        let update = match self.watch.refusal() {
            Some(refusal) => Some(refused(&refusal)),
            None => snapshot.intent.as_ref().map(|intent| {
                let seq = self.watch.seq_of(&intent.update_id);
                recorded(intent, seq)
            }),
        };
        Body {
            proxy_boot: self.boot.clone(),
            installed_commit: installed,
            active_slot: snapshot.slot,
            active_commit: snapshot.running,
            update,
            pending_binary: self.watch.pending(),
        }
    }
}

#[async_trait]
impl Answer for SwapStatus {
    async fn answer(&self, call: Call) -> Answered {
        if call.path != PATH {
            return Gone.answer(call).await;
        }
        if call.method != "GET" {
            return Answered {
                status: 405,
                headers: vec![(ALLOW, HeaderValue::from_static("GET"))],
                body: NOT_ALLOWED.as_bytes().to_vec(),
            };
        }
        let source = Arc::clone(&self.source);
        let orders = Arc::clone(&self.orders);
        let read = move || (source.snapshot(), orders.installed().ok().flatten());
        let joined = tokio::task::spawn_blocking(read).await;
        let (snapshot, installed) = joined.expect("reading the swap does not panic");
        let body = self.body(snapshot, installed);
        Answered {
            status: 200,
            headers: Vec::new(),
            body: serde_json::to_vec(&body).expect("the answer serializes"),
        }
    }
}

#[cfg(test)]
#[path = "swap_status_tests.rs"]
mod tests;
