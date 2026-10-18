//! What a farm does when another process takes its work over (`crate::handover`): it
//! stops taking any ([`Farm::fence`]), ends the worker streams so the daemons reconnect to
//! the successor ([`Farm::end_worker_streams`]), answers the callers it still holds from
//! what the successor writes to the state store ([`Farm::follow`]), and keeps an
//! operator's cordons across the change.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use kbf_front::{Handed, Stage};
use kbf_sched::{Event, Input};
use kbf_types::{Answer, Digest, FarmTime, OperationId, StateMachine as _, WaiterId, WorkerId};
use tokio::sync::watch;
use tonic::Status;

use super::durable::{Durable, Progress, StateError, StoredCordon};
use super::{Farm, NodeAction, State, unix_ms};

/// How many looks in a row at the state store may find an operation neither unfinished
/// nor ended before it is reported as untraced (a WARN, and `kbf_frontend_relay_untraced`).
/// A writer ends an operation in two transactions, the second moments after the first, so
/// one look can fall between them; a successor that died between them leaves the
/// operation like that for good. Only a report: an untraced operation's callers are not
/// answered, they keep waiting (see [`Farm::follow`]).
pub const UNTRACED_LOOKS: u32 = 2;

/// Why a request that raced the handover is refused (UNAVAILABLE: Buck2 asks again, and
/// the successor answers).
const HANDING_OVER: &str = "this frontend is handing its work to its successor";

/// How often [`Farm::flush_unsettled`] writes again what the state store refused.
const FLUSH_LOOK: Duration = Duration::from_millis(10);

/// One `Result` being taken, counted until dropped.
pub(super) struct Taking<'a>(&'a AtomicUsize);

impl Drop for Taking<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl NodeAction {
    /// How the cordon row keeps this action, `at` Unix milliseconds: `None` when the node
    /// returns to placement.
    pub(super) fn stored(self, at: u64) -> Option<StoredCordon> {
        match self {
            Self::Cordon => Some(StoredCordon::Cordoned),
            Self::Drain(within) => Some(StoredCordon::Draining {
                deadline_unix_ms: at.saturating_add(millis(within)),
            }),
            Self::Uncordon => None,
        }
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl Farm {
    /// Counts a `Result` as being taken until the guard drops.
    pub(super) fn in_flight(&self) -> Taking<'_> {
        self.reports.fetch_add(1, Ordering::SeqCst);
        Taking(&self.reports)
    }

    /// Whether nothing is half done that a handover should not cut: no `Result` is being
    /// taken and no Execute is between its request and its queueing (the input check).
    #[must_use]
    pub fn quiet(&self) -> bool {
        let presubmit = self.cache.metrics().execute_presubmit_in_flight.get();
        let taking = u64::try_from(self.reports.load(Ordering::SeqCst)).unwrap_or(u64::MAX);
        presubmit + taking == 0
    }

    /// Takes this farm out of service for good, for a successor to take over: from now
    /// on it feeds the scheduler no input (a `Result` is refused, nothing is granted or
    /// given up, a heartbeat changes nothing), queues no request, and writes nothing a
    /// successor's work could depend on. What it already holds stays as it was, for
    /// [`Farm::follow`] to answer.
    pub fn fence(&self) {
        self.lock().fenced = true;
    }

    /// Writes, to the state store, the requeues it refused (a lease the scheduler gave
    /// up while the store did not take the write), so that the successor recovers each
    /// operation queued and not under the lease this process gave up. Works on a fenced
    /// farm, which feeds the scheduler nothing and so would never write them again.
    /// Tries again every [`FLUSH_LOOK`] until none is left, and stops at once when the
    /// store says a newer writer holds it (nothing may be written then), or at the
    /// `deadline`; whatever is left is logged at ERROR, naming the operations. Returns
    /// the operations whose requeues were not written.
    pub async fn flush_unsettled(&self, deadline: Duration) -> Vec<OperationId> {
        let until = tokio::time::Instant::now() + deadline;
        let (left, why) = loop {
            // The lock is not held across the sleep.
            let step = self.lock().flush_step();
            match step {
                Flushed::Settled => return Vec::new(),
                Flushed::Superseded(left) => break (left, "the state store has a newer writer"),
                Flushed::Refused(left) => {
                    if tokio::time::Instant::now() >= until {
                        break (
                            left,
                            "the state store kept refusing them until the deadline",
                        );
                    }
                    tokio::time::sleep(FLUSH_LOOK).await;
                }
            }
        };
        let operations: Vec<String> = left.iter().map(|op| format!("{:016x}", op.0)).collect();
        tracing::error!(
            count = left.len(),
            operations = %operations.join(","),
            why,
            "requeues not written to the state store: the successor recovers these operations \
             with the lease they held"
        );
        left
    }

    /// Ends every worker stream with `status`, so each daemon dials again (to the
    /// successor, once this process no longer accepts). Returns how many streams were
    /// still open.
    pub fn end_worker_streams(&self, status: &Status) -> usize {
        let state = self.lock();
        state
            .links
            .values()
            .filter(|link| link.outbound.send(Err(status.clone())).is_ok())
            .count()
    }

    /// Answers, from the state store, the callers this fenced farm still holds: an
    /// operation the successor finished gets its result from the `done/` row, one it has
    /// leased is shown executing. A caller waits until its operation finishes, it cancels
    /// (its stream is dropped), or this process is killed: no look count or timer ends
    /// it. An operation the store has no trace of keeps its callers waiting; it is
    /// reported ([`UNTRACED_LOOKS`], `kbf_frontend_relay_untraced`) so an operator sees
    /// it. Call it every so often; returns how many callers are still waiting.
    ///
    /// # Errors
    /// The state store cannot be read.
    pub fn follow(&self) -> Result<usize, StateError> {
        let waiting = self.lock().follow()?;
        self.report_untraced();
        Ok(waiting)
    }

    /// Sets `kbf_frontend_relay_untraced` to the operations the last looks found no trace
    /// of.
    fn report_untraced(&self) {
        let untraced = self.lock().untraced();
        self.cache
            .metrics()
            .frontend_relay_untraced
            .set(untraced as u64);
    }
}

/// How an attempt to write the requeues the state store refused ended.
pub(super) enum Flushed {
    /// Every requeue is written.
    Settled,
    /// A newer writer holds the store: these operations' requeues will not be written.
    Superseded(Vec<OperationId>),
    /// The store refused again: these operations' requeues are still unwritten.
    Refused(Vec<OperationId>),
}

impl State {
    /// Writes the requeues the state store has not taken, and the ones the scheduler has
    /// given up since the last feed, once. Works on a fenced farm: this is the one thing
    /// a fenced farm writes, because the successor's recovery depends on it (see
    /// [`Farm::flush_unsettled`]).
    pub(super) fn flush_step(&mut self) -> Flushed {
        let superseded = |state: &Self| state.durable.as_ref().is_some_and(Durable::superseded);
        if !superseded(self) {
            self.log_requeues();
        }
        let left: Vec<OperationId> = self
            .unsettled
            .iter()
            .map(|(requeue, ..)| requeue.operation)
            .collect();
        match (left.is_empty(), superseded(self)) {
            (true, _) => Flushed::Settled,
            (false, true) => Flushed::Superseded(left),
            (false, false) => Flushed::Refused(left),
        }
    }

    /// Feeds `event` to the scheduler (see [`State::carry_out`]), unless the farm is
    /// fenced: then nothing is fed and there are no answers.
    pub(super) fn feed(&mut self, now: FarmTime, event: Event) -> Vec<Answer> {
        if self.fenced {
            return Vec::new();
        }
        self.carry_out(now, event)
    }

    /// Writes `request`, about to be queued, to the state store (see
    /// [`State::queue`]), unless the farm is fenced.
    ///
    /// # Errors
    /// UNAVAILABLE if the farm is fenced or the store did not take it.
    pub(super) fn admit(&self, request: &kbf_sched::Request) -> Result<(), Status> {
        if self.fenced {
            return Err(Status::unavailable(HANDING_OVER));
        }
        self.queue(request)
    }

    /// The callers of `operation` still waiting: not answered, and with a stream open
    /// (a caller that cancelled has dropped its end of the channel).
    fn callers_waiting(&self, operation: OperationId) -> Vec<WaiterId> {
        let waiting = |caller: &WaiterId| {
            let stage = self.stages.get(caller);
            stage.is_some_and(|stage| {
                !stage.is_closed() && !matches!(*stage.borrow(), Stage::Done(_))
            })
        };
        let callers = self.sched.waiters(operation).unwrap_or_default();
        callers.iter().copied().filter(waiting).collect()
    }

    /// [`Farm::follow`] under the lock: how many callers still wait.
    pub(super) fn follow(&mut self) -> Result<usize, StateError> {
        let Some(durable) = &self.durable else {
            return Ok(0);
        };
        let mut looked = Vec::new();
        for operation in self.sched.unfinished() {
            let callers = self.callers_waiting(operation);
            if !callers.is_empty() {
                looked.push((operation, callers, durable.progress(operation)?));
            }
        }
        // An operation nobody waits on any more (its callers cancelled) has nothing to
        // report.
        self.untraced_kept
            .retain(|operation, _| looked.iter().any(|(looked, ..)| looked == operation));
        let mut waiting = 0;
        for (operation, callers, progress) in looked {
            let stage = note(&mut self.untraced_kept, operation, &progress);
            let ended = matches!(stage, Some(Stage::Done(_)));
            waiting += callers.len() * usize::from(!ended);
            if let Some(stage) = stage {
                for channel in callers.iter().filter_map(|caller| self.stages.get(caller)) {
                    show(channel, stage.clone());
                }
            }
        }
        Ok(waiting)
    }

    /// How many operations the last looks found no trace of, [`UNTRACED_LOOKS`] times in
    /// a row, among the ones this farm's callers wait on and the ones relays follow.
    pub(super) fn untraced(&self) -> usize {
        let reported = |looks: &&u32| **looks >= UNTRACED_LOOKS;
        let kept = self.untraced_kept.values().filter(reported).count();
        kept + self.untraced_relayed.values().filter(reported).count()
    }

    /// Takes up the cordons of the nodes an operator placed before this process started,
    /// as the store holds them: a drain keeps its wall-clock deadline (already past:
    /// paused at the first tick).
    pub(super) fn restore_cordons(&mut self, cordons: Vec<(WorkerId, StoredCordon)>) {
        let (now, at) = (FarmTime::default(), unix_ms());
        for (worker, cordon) in cordons {
            let event = match cordon {
                StoredCordon::Cordoned => Event::Cordon { worker },
                StoredCordon::Draining { deadline_unix_ms } => Event::Drain {
                    worker,
                    deadline: now
                        .saturating_add(Duration::from_millis(deadline_unix_ms.saturating_sub(at))),
                },
            };
            let none = self.sched.apply(Input::new(now, event));
            debug_assert!(none.is_empty(), "a cordon has no effect: {none:?}");
        }
    }
}

impl Progress {
    /// What the callers of an operation at this point are shown: `None` leaves them as
    /// they are (queued, or no trace: they wait).
    fn stage(&self) -> Option<Stage> {
        match self {
            Self::Ended(finished) => Some(Stage::Done(finished.clone())),
            Self::Running => Some(Stage::Executing),
            Self::Queued | Self::Gone => None,
        }
    }
}

/// One look at `operation`, whose callers wait: what they are shown now (`None` leaves
/// them as they are). An operation the store has no trace of is shown nothing: it is
/// counted in `untraced`, reported once it has been untraced for [`UNTRACED_LOOKS`] looks
/// in a row, and its callers go on waiting. A look that finds it starts the count over.
fn note(
    untraced: &mut BTreeMap<OperationId, u32>,
    operation: OperationId,
    progress: &Progress,
) -> Option<Stage> {
    if progress.is_gone() {
        let looks = untraced.entry(operation).or_default();
        *looks = looks.saturating_add(1);
        if *looks == UNTRACED_LOOKS {
            tracing::warn!(%operation, "no trace in the state store: callers keep waiting");
        }
    } else {
        untraced.remove(&operation);
    }
    progress.stage()
}

/// Shows `stage` on `channel`, waking its watchers only if it changed.
fn show(channel: &watch::Sender<Stage>, stage: Stage) {
    channel.send_if_modified(|now| {
        let changed = *now != stage;
        if changed {
            *now = stage;
        }
        changed
    });
}

impl Handed for Farm {
    fn handed(&self) -> bool {
        self.lock().fenced
    }

    /// Reads the unfinished rows of the state store (one scan per relayed call, made while
    /// the successor's first operation arrives); a farm with no store finds none.
    fn operation_of(
        &self,
        instance: Option<&str>,
        action: &Digest,
    ) -> Result<Option<OperationId>, String> {
        let state = self.lock();
        let Some(durable) = &state.durable else {
            return Ok(None);
        };
        let found = durable
            .operations_of(instance, action)
            .map_err(|error| error.to_string())?;
        Ok(match found[..] {
            [only] => Some(only),
            _ => None,
        })
    }

    /// The follower ([`Farm::follow`]) for one operation this farm's scheduler never
    /// held: the same looks, and the same rule for an operation the store has no trace of
    /// (nothing is shown, it is reported, the call keeps waiting).
    fn look(&self, operation: OperationId) -> Result<Option<Stage>, String> {
        let mut state = self.lock();
        let Some(durable) = &state.durable else {
            return Ok(None);
        };
        let progress = durable
            .progress(operation)
            .map_err(|error| error.to_string())?;
        let stage = note(&mut state.untraced_relayed, operation, &progress);
        drop(state);
        self.report_untraced();
        Ok(stage)
    }

    fn release(&self, operation: OperationId) {
        self.lock().untraced_relayed.remove(&operation);
        self.report_untraced();
    }
}
