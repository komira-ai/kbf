//! A worker's `Result` from the report to the answer: what the daemon reported, the
//! scheduler's outcome for it, and what the callers of the operation it finishes are told.

use kbf_front::{Finished, Stage};
use kbf_meta::ActionRecord;
use kbf_proto::worker::{
    self, ResultAck, ResultAckReason as Reason, ServerMessage, server_message,
};
use kbf_sched::Event;
use kbf_types::{
    Answer, Digest, Failure, FarmTime, LeaseId, OperationId, Outcome, WaiterId, WorkerId,
};
use tokio::sync::watch;

use super::ack::{self, Verdict, refusal};
use super::failed_actions::class_of;
use super::{Farm, Outbound, State, wire_lease};
use crate::action_usage::{self, Measured};
use crate::answer;
pub(super) use crate::answer::{Detail, Pending, finished_of, refused_as};
#[cfg(test)]
pub(super) use crate::answer::{failed, unrun};
use crate::machine::Waiter;
use crate::memory;
use crate::run_record;

/// What a finished operation tells its callers, and the action-cache entry to write
/// first, if any.
pub(super) struct Settled {
    pub(super) finished: Finished,
    write: Option<(Digest, ActionRecord)>,
    stages: Vec<watch::Sender<Stage>>,
    /// The acknowledgement a `Result` was owed until this finish was written, and the
    /// stream to send it on (`Engine::owed`).
    ack: Option<(Outbound, ResultAck)>,
}

/// What a `Result` the scheduler was fed is acknowledged with ([`State::take_report`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Taken {
    /// It became the operation's result, or a memory kill or lost run the scheduler took.
    Accepted,
    /// The scheduler did not take it, for this reason.
    Refused(Reason),
    /// The scheduler took it, but its finish is not written yet: acknowledged once it is.
    Held,
    /// The scheduler took it, and the record was dropped unwritten: never acknowledged.
    Dropped,
    /// It names the operation's current lease, and the scheduler took no answer from it:
    /// never acknowledged, so the daemon keeps it and sends it again. A refusal would lose
    /// it, and leave the lease on the node.
    Unanswered,
}

impl Taken {
    /// The `accepted` and `reason` of the `ResultAck` this is answered with; `None` for no
    /// ack at all.
    pub(super) const fn ack(self) -> Option<(bool, Reason)> {
        match self {
            Self::Accepted => Some((true, Reason::Accepted)),
            Self::Refused(reason) => Some((false, reason)),
            Self::Held | Self::Dropped | Self::Unanswered => None,
        }
    }
}

/// What a `Result` the scheduler gave no answer to is taken as, from what its lease stands
/// for now (`standing`, `Ok` while it is the operation's current lease) and whether the farm
/// is fenced.
///
/// A current lease the scheduler took no answer from is not acknowledged, so that the daemon
/// keeps the `Result` and sends it again: a refusal would lose it, and leave the lease on the
/// node. A fenced farm feeds nothing, so it refuses every `Result` as stale, the current
/// lease's too, by design.
pub(super) fn not_answered(
    standing: Result<(), Verdict>,
    fenced: bool,
    operation: OperationId,
    lease: LeaseId,
    worker: &WorkerId,
) -> Taken {
    match standing {
        Ok(()) if fenced => {
            ack::log_refusal(worker, lease, Reason::Stale);
            Taken::Refused(Reason::Stale)
        }
        Ok(()) => {
            tracing::error!(
                %operation, %lease, %worker,
                "the scheduler took no answer from the result of the operation's current lease; not acknowledged, so the daemon sends it again"
            );
            Taken::Unanswered
        }
        Err(Verdict::Held) => Taken::Held,
        Err(Verdict::Refused(Reason::Duplicate)) => Taken::Dropped,
        Err(Verdict::Refused(reason)) => {
            ack::log_refusal(worker, lease, reason);
            Taken::Refused(reason)
        }
    }
}

/// A worker's report as [`State::take_report`] feeds it: the lease, the scheduler's
/// outcome, and what the report measured for the `action usage:` line and for the
/// invocation record (`None` for an operation no recorded call waits on).
pub(super) struct Report {
    pub(super) lease: LeaseId,
    pub(super) outcome: Outcome,
    pub(super) measured: Option<Measured>,
    /// The exit code of the action's run, if the worker reported one ([`action_usage::exit_code`]).
    pub(super) exit_code: Option<i32>,
    pub(super) run: Option<kbf_invocation::Run>,
}

impl Farm {
    /// A `Result` from `worker`. Accepted only if `worker` holds the operation's
    /// current lease, which this process granted, and the `Result` names no action but
    /// the one that lease runs; an accepted OK result is written to the action cache
    /// (unless the action is `do_not_cache` or exited non-zero) before its callers are
    /// answered. Before it is checked, its `ActionResult` gets the node id and the
    /// server's times ([`Stamp::apply`]), so callers and the cache see them. Returns
    /// the acknowledgement, or `None` for a `Result` without a lease id, and for one the
    /// state store has not taken yet or the farm dropped over its cap: a daemon forgets a
    /// `Result` on its ack, so none is sent for a record that is not durable. A held
    /// result is acknowledged when its record is written ([`State::settle`]); the daemon
    /// resends what it is not acknowledged for, and a copy of a held result gets no ack
    /// either.
    pub async fn report(&self, worker: &WorkerId, result: worker::Result) -> Option<ResultAck> {
        js_arm_async!(self, hot::report(worker, result));
        let wire_lease = result.lease_id?;
        let _taking = self.in_flight();
        let lease = LeaseId::new(wire_lease.term, wire_lease.seq);
        let (standing, recorded, booked) = {
            let mut state = self.lock();
            let standing = state.standing(lease, worker);
            let recorded = standing
                .as_ref()
                .is_ok_and(|(operation, ..)| state.side.recording.is_recorded(*operation));
            // A copy of a result the scheduler took whose finish waits for the store is
            // acknowledged with the first, to the node that sent the first: a copy from
            // another node, which never held the lease, is owed nothing.
            if matches!(standing, Err(Verdict::Held)) {
                state.engine.owed.entry(lease).or_insert(worker.clone());
            }
            let booked = state.side.machine.sent(lease).and_then(|sent| sent.booked);
            (standing, recorded, booked)
        };
        let (operation, action, stamp) = match standing {
            Ok(holding) => holding,
            Err(Verdict::Held) => {
                tracing::info!(%worker, %lease, "result held: its record is not written yet; no ack");
                return None;
            }
            Err(Verdict::Refused(reason)) => {
                ack::log_refusal(worker, lease, reason);
                return Some(refusal(wire_lease, reason));
            }
        };
        // The lease id is this process's, so the run it reports was started for this
        // operation; a daemon that says it ran another action is refused all the same
        // (issue #137).
        if let Some(named) = &result.action_digest
            && *named != kbf_front::digest_to_proto(&action)
        {
            ack::log_refusal(worker, lease, Reason::Mismatch);
            return Some(refusal(wire_lease, Reason::Mismatch));
        }
        let ran_on = (worker, stamp);
        // Read before `outcome`, which drops a failed run's result.
        let measured = Measured::of(&result).map(|m| m.booked(booked));
        let exit_code = action_usage::exit_code(&result);
        let run = recorded.then(|| {
            let usage = measured.as_ref().map(Measured::usage);
            run_record::of(&result, usage, stamp, self.ambient.wall(), booked)
        });
        let wall = || self.ambient.wall();
        let (outcome, detail) = answer::outcome(&self.cache, wall, lease, ran_on, result).await;

        let now = self.now();
        let (answers, taken) = {
            let mut state = self.lock();
            let report = Report {
                lease,
                outcome,
                measured,
                exit_code,
                run,
            };
            // Parked before the feed: the transaction that finishes the operation writes
            // the result these details make. An answer the store held back earlier,
            // released by this input, is settled with its own detail.
            if let Some(detail) = detail {
                state.park(lease, detail);
            }
            let (answers, taken) = state.take_report(now, worker, operation, report);
            if taken == Taken::Held {
                state.engine.owed.insert(lease, worker.clone());
            }
            let settled: Vec<Settled> = answers.iter().map(|a| state.settle(a)).collect();
            // What no answer used waits only if its answer does.
            state.forget_parked();
            (settled, taken)
        };
        for settled in answers {
            self.deliver(settled).await;
        }
        // None: the ack is sent when the store has taken the record, or never.
        let (accepted, reason) = taken.ack()?;
        Some(ResultAck {
            lease_id: Some(wire_lease),
            accepted,
            reason: reason as i32,
        })
    }

    /// Writes the action-cache entry, if any, then answers the callers.
    pub(super) async fn deliver(&self, settled: Settled) {
        if let Some((action, record)) = settled.write
            && let Err(e) = self.cache.commit_action_record(action, record).await
        {
            tracing::error!(%action, error = %e, "action-cache write of an accepted result failed");
        }
        for stage in settled.stages {
            stage.send_replace(Stage::Done(settled.finished.clone()));
        }
        if let Some((outbound, ack)) = settled.ack {
            // A stream that has just ended drops it; the daemon sends the result again.
            let message = server_message::Message::ResultAck(ack);
            let _ = outbound.send(Ok(ServerMessage {
                message: Some(message),
            }));
        }
    }
}

impl State {
    /// How many runs of `operation` the scheduler has recorded as killed for memory or
    /// as the farm's fault.
    fn runs_recorded(&self, operation: OperationId) -> usize {
        let memory = self
            .engine
            .sched
            .memory_runs(operation)
            .map_or(0, <[_]>::len);
        let farm = self.engine.sched.farm_runs(operation).map_or(0, <[_]>::len);
        memory + farm
    }

    /// Feeds `worker`'s report that `lease` of `operation` ended with `outcome`, and
    /// returns the answers it led to and whether the scheduler took it. A busy node's
    /// memory kill the scheduler took is logged for operators (`crate::memory`), and a
    /// report the scheduler took that `measured` what the lease used has its `action
    /// usage:` line ([`action_usage`]).
    ///
    /// The in-process log commits its record at once: an accepted result is answered
    /// here, and a refused one (its lease given up while its outputs were checked)
    /// never is. A memory kill or lost run the scheduler takes is answered, or records
    /// a run and runs the operation again. The replicated log will answer once the
    /// record commits, carrying the result with it.
    ///
    /// A recorded operation's report the scheduler took has its measurement sent to
    /// the invocation recorder, with the booking of a memory kill.
    ///
    /// Not generic, unlike `Farm::report`, so every test binary's runs count toward the
    /// same function's coverage.
    pub(super) fn take_report(
        &mut self,
        now: FarmTime,
        worker: &WorkerId,
        operation: OperationId,
        report: Report,
    ) -> (Vec<Answer>, Taken) {
        let Report {
            lease,
            outcome,
            measured,
            exit_code,
            run,
        } = report;
        // Before the feed: an own-limit kill's requeue writes what it taught the
        // estimator in its own transaction.
        self.side.stage_learning(
            &self.engine.sched,
            operation,
            lease,
            worker,
            outcome,
            measured.as_ref(),
        );
        // Taken first: a report that finishes the operation may drop its callers.
        let usage = measured.and_then(|measured| {
            let (_, waiter) = self.side.first_waiter(&self.engine.sched, operation)?;
            Some((waiter.usage.clone(), measured))
        });
        let recorded_before = self.runs_recorded(operation);
        let name = self.operation_name(operation);
        let event = Event::Report {
            operation,
            lease,
            outcome,
        };
        let answers = self.feed(now, event);
        let rerun = self.runs_recorded(operation) > recorded_before;
        let accepted = rerun || answers.iter().any(|a| a.lease == lease);
        // Not answered: refused, or finished by this lease with the finish waiting for the
        // store, or with its record dropped unwritten (a finish neither held nor answered),
        // or still the current lease, which the scheduler took no answer from.
        let taken = if accepted {
            Taken::Accepted
        } else {
            let standing = self.standing(lease, worker).map(drop);
            not_answered(standing, self.engine.fenced, operation, lease, worker)
        };
        self.finish_learning(operation, lease, worker, accepted || taken == Taken::Held);
        if accepted && let Some((subject, measured)) = &usage {
            action_usage::log(operation, subject, measured, outcome, lease, worker);
        }
        if accepted && let Some(run) = run {
            let killed = match outcome {
                Outcome::Failed(Failure::OutOfMemory | Failure::NodeMemoryPressure) => answers
                    .iter()
                    .find(|a| a.lease == lease)
                    .map(|a| a.memory_runs.as_slice())
                    .or_else(|| self.engine.sched.memory_runs(operation))
                    .and_then(<[_]>::last)
                    .filter(|r| r.worker == *worker),
                _ => None,
            };
            let run = match killed {
                Some(killed) => run_record::killed(run, killed),
                None => run,
            };
            self.side.recording.ran(operation, lease, run);
        }
        let (operation, node) = (name, worker);
        // `-` when the worker reported no run (a failure before the action executed).
        let exit_code = exit_code.map_or_else(|| "-".to_owned(), |code| code.to_string());
        match outcome {
            Outcome::Completed { .. } => {
                tracing::info!(%operation, %lease, %node, accepted, %exit_code, "lease finished ok");
            }
            Outcome::Failed(failure) => {
                tracing::warn!(%operation, %lease, %node, ?failure, accepted, %exit_code, "lease finished failed");
            }
        }
        if accepted && outcome == Outcome::Failed(Failure::NodeMemoryPressure) {
            memory::pressure(worker, self.engine.sched.memory_pressure(worker));
        }
        (answers, taken)
    }

    /// The REAPI name of `operation`, as the log names it.
    fn operation_name(&self, operation: OperationId) -> String {
        self.side.operation_name(&self.engine.view(), operation)
    }

    /// Keeps the callers of `operation`, which has just finished, until the scheduler
    /// drops it. Its result is in the state store already: it was written in the
    /// transaction that removed the operation's rows (`Durable::persist`), before any
    /// caller is answered from this process's memory (a process that kept streams open
    /// across a handover reads it there).
    fn keep_finished(&mut self, operation: OperationId, waiters: Vec<WaiterId>) {
        self.side.machine.keep_finished(operation, waiters);
        self.forget_dropped();
    }

    /// Keeps `detail`, the report of `lease`, before the report is fed: the answer it
    /// decides is written with the result it carries, in the transaction that finishes
    /// the operation (`Engine::ending`), and settled with the same detail
    /// ([`State::settle`]). Call [`State::forget_parked`] once the answers are settled.
    pub(super) fn park(&mut self, lease: LeaseId, detail: Detail) {
        self.side.parked.insert(lease, detail);
    }

    /// Forgets the detail of every report whose answer is no longer held.
    fn forget_parked(&mut self) {
        self.engine.forget_parked(&mut self.side);
    }

    /// Works out what the callers of a finished operation get, and keeps them for the
    /// finished retention. The report's detail, if there is one, is the one parked for the
    /// answer's lease ([`State::park`]), the one its `done/` row was made from.
    pub(super) fn settle(&mut self, answer: &Answer) -> Settled {
        let detail = self.side.parked.remove(&answer.lease);
        self.engine.answered.insert(answer.lease, answer.operation);
        self.side.machine.forget_leases(answer.operation);
        let waiters: Vec<&Waiter> = answer
            .waiters
            .iter()
            .filter_map(|id| self.side.machine.waiter(*id))
            .collect();
        let stages = answer
            .waiters
            .iter()
            .filter_map(|id| self.side.stages.get(id).cloned())
            .collect();
        let reason = detail.as_ref().and_then(Detail::reason);
        if let Some(class) = class_of(answer.outcome, detail.as_ref()) {
            self.side.failed.count(class);
        }
        let finished = finished_of(answer, detail.as_ref());
        let (stored, write) = match detail {
            Some(Detail::Ran(pending)) => {
                let Pending { result, record } = *pending;
                let cacheable =
                    result.exit_code == 0 && waiters.first().is_some_and(|w| !w.do_not_cache);
                let stored = Some(record.result);
                let write = waiters
                    .first()
                    .filter(|_| cacheable)
                    .map(|w| (w.key.action, record));
                (stored, write)
            }
            _ => (None, None),
        };
        self.side
            .recording
            .finished(answer, &finished, stored, reason);
        self.keep_finished(answer.operation, answer.waiters.clone());
        // A result whose finish waited for the store is acknowledged to its sender's
        // newest stream: the store has taken the row.
        let ack = self
            .engine
            .owed
            .remove(&answer.lease)
            .and_then(|worker| self.side.links.get(&worker))
            .map(|link| {
                let ack = ResultAck {
                    lease_id: Some(wire_lease(answer.lease)),
                    accepted: true,
                    reason: Reason::Accepted as i32,
                };
                (link.outbound.clone(), ack)
            });
        Settled {
            finished,
            write,
            stages,
            ack,
        }
    }
}
