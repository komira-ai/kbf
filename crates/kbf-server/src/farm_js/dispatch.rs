//! The farm's calls on a core: what a `Hello`, a submit, a name a client asks for and a
//! `Result` are made of, between the worker and front services and
//! [`Core`](js_server_core::Core), with [`KbfAdapter`] in it.
//!
//! These are the bodies of [`Farm::register`](crate::Farm::register),
//! [`Farm::resize`](crate::Farm::resize), `Dispatch::submit`, `Dispatch::wait` and
//! [`Farm::report`](crate::Farm::report) that are the farm's own and not the scheduler's:
//! the messages of `kbf.worker.v1`, the REAPI names and the `ActionResult`. What the core
//! decides (queueing, placement, the order of rows and effects, a heartbeat, a tick, an
//! operator's action) it is asked for directly.
//!
//! Left to the host, because the core has no hook for them: the `action usage:` line of a
//! report, the invocation record's measurement of it ([`Recording::ran`](crate::run_record::Recording::ran)) and the
//! node memory pressure line (all made after the feed in the farm's own driver, from the
//! answer the feed returned), and the scheduler gauges of `/metrics`.

use js_server_core::Reason as Refused;
use js_server_core::{
    Admitted, Core, Denied, NotQueued, Report, Reported as CoreReported, Standing, Stream, Taken,
    View,
};
use js_store::{Ext as _, Progress};
use kbf_front::{Backend, Stage, Submission, Ticket};
use kbf_proto::worker::{self, ResultAck, ResultAckReason};
use kbf_sched::DaemonInstance;
use kbf_types::{ActionKey, FarmTime, LeaseId, OperationId, Outcome, WorkerId};
use tonic::Status;

use super::adapter::{KbfAdapter, Wall, wire_lease};
use super::ext::KbfExt;
use super::host::{Caller, Connection};
use super::learning::{Ask, Sized};
use crate::action_usage::{Measured, Shapes};
use crate::answer;
use crate::farm::Reported;
use crate::farm::durable::{named, subject};
use crate::machine::{Platform, Sent, parse_stored_name, stored_name};
use crate::run_record;
use crate::stamp::Stamp;

/// `reported` as the core takes a `Hello`: the protocol version it names is the stream's
/// own, which the core does not keep.
fn core_reported(reported: Reported) -> CoreReported {
    CoreReported::serving(reported.capacity, reported.caps).in_phase(reported.phase)
}

/// The first `Hello` of a stream (see [`Core::register`]): `connection` becomes the worker's
/// newest stream, and its `Welcome` is sent.
///
/// # Errors
/// The core's admission refuses the host.
pub fn register(
    core: &Core<KbfAdapter>,
    now: FarmTime,
    worker: &WorkerId,
    instance: DaemonInstance,
    predecessor: DaemonInstance,
    reported: Reported,
    connection: Connection,
) -> Result<Stream, Denied> {
    let noted = reported.clone();
    let named = instance.as_str().to_owned();
    let open = |adapter: &mut KbfAdapter, stream| {
        adapter.attach(worker, stream, connection);
        adapter.note_registered(worker, &noted);
        adapter.note_instance(worker, &named);
    };
    core.register(
        now,
        worker,
        instance,
        predecessor,
        core_reported(reported),
        open,
    )
}

/// A `Hello` resent on `stream` (see [`Core::resize`]).
///
/// # Errors
/// The core's admission refuses the host.
pub fn resize(
    core: &Core<KbfAdapter>,
    now: FarmTime,
    worker: &WorkerId,
    stream: Stream,
    reported: Reported,
) -> Result<(), Denied> {
    let caps = reported.caps.clone();
    let update = |adapter: &mut KbfAdapter| adapter.note_resized(worker, &caps);
    core.resize(now, worker, stream, core_reported(reported), update)
}

/// The refusal of a request the core did not queue, as the front is told it.
fn unavailable(not_queued: NotQueued) -> Status {
    match not_queued {
        NotQueued::Fenced => {
            Status::unavailable("this frontend is handing its work to its successor")
        }
        NotQueued::Unavailable => {
            Status::unavailable("the scheduler's state store did not take the request")
        }
        NotQueued::Finishing => {
            Status::unavailable("the action's previous run is finishing; the request is not queued")
        }
        NotQueued::Superseded => Status::failed_precondition(
            "this scheduler was superseded by a newer writer of the state store",
        ),
    }
}

/// Queues `submission`, or joins it to the operation in flight for it, for a caller of its
/// own, and returns the caller's ticket (see [`Core::submit`]).
///
/// # Errors
/// INVALID_ARGUMENT for a request that does not key an action; UNAVAILABLE if the core
/// is handing its work over, or the state store did not take the request; FAILED_PRECONDITION
/// if a newer writer holds the store.
pub fn submit(
    core: &Core<KbfAdapter>,
    now: FarmTime,
    shapes: &Shapes,
    mut submission: Submission,
) -> Result<Ticket, Status> {
    // The front keys every request by an action; the callers and tickets of the server
    // name that action.
    let key = ActionKey::try_from(&submission.request.key)
        .map_err(|e| Status::invalid_argument(e.to_string()))?;
    // Outside the lock: hashes the action's shape and drops its argv and paths.
    let usage = shapes.subject(std::mem::take(&mut submission.described));
    let shape = submission.call.as_ref().map(|_| usage.recorded());
    let sized = Sized {
        kind: submission.request.kind,
        gpus: submission.request.resources.gpus,
        memory_bytes: submission.request.resources.memory_bytes,
        cpu_millis: submission.request.resources.cpu_millis,
        evidence: submission.evidence,
    };
    let call = submission
        .call
        .take()
        .zip(shape)
        .map(|(call, shape)| call.with_shape(shape).with_evidence(submission.evidence));
    let parcel = subject::encode(&usage);
    let (instance, action) = (key.instance.clone(), key.action);
    let caller = Caller {
        key,
        kind: submission.request.kind,
        do_not_cache: submission.request.do_not_cache,
        usage,
        platform: Platform::of(&submission.request.needs),
    };
    // The prediction (`--placement-predictions`) is asked for in a hold of the core of its own,
    // before the submit's: the core charges it from the operation's first placement round.
    // `shadow` hands the core nothing.
    let (forecast, on) = core.with(|view, adapter| {
        let ask = Ask {
            request: &submission.request,
            instance: &caller.key.instance,
            platform: &caller.platform,
            subject: &caller.usage,
        };
        (
            adapter.forecast_submit(view, &sized, ask),
            adapter.hands_predictions(),
        )
    });
    let prediction = forecast.and_then(Result::ok).filter(|_| on);
    let (sender, stage) = tokio::sync::watch::channel(Stage::Queued);
    let enrol = |adapter: &mut KbfAdapter, admitted: &Admitted| {
        adapter.enrol(admitted, caller, call, sender);
    };
    let admitted = core
        .submit_with(now, submission.request, &parcel, None, prediction, enrol)
        .map_err(unavailable)?;
    // A caller that joined an operation that holds a lease sees it executing.
    core.with(|view, adapter| {
        adapter.mark_joined(view, &admitted);
        adapter.learn_submit(view, &admitted, &sized, forecast);
    });
    Ok(Ticket {
        name: stored_name(admitted.operation),
        instance,
        action,
        stage,
    })
}

/// A new ticket on the operation called `name`: through the first caller the core holds for
/// it, if it holds the operation, and from the state store if not. `None` if `name` is not
/// of the form `operations/<id>`, or neither the core nor the store has a trace of the
/// operation (it never existed, or its result is past its retention), and for one whose
/// `done/` row cannot be read (logged and counted at each look). The by-name lookup answers
/// not found there, unlike the callers already waiting (UNAVAILABLE, see
/// `KbfAdapter::decoded`): a client that resumes by name and is told UNAVAILABLE retries
/// the same name, which reads the same bytes until the result expires, while not found makes
/// it send a fresh Execute call.
pub fn wait(core: &Core<KbfAdapter>, name: &str) -> Option<Ticket> {
    let operation = parse_stored_name(name)?;
    let held = core.with(|view, adapter| adapter.held_ticket(view, operation));
    held.or_else(|| stored_ticket(core, operation))
}

/// A ticket on `operation`, which the core does not hold, through a channel of its own,
/// shown what the store says of the operation now and at each intake poll while a ticket
/// holds it. The action is read first: an operation finishes in one transaction that removes
/// its `op/` row and puts its `doneof/` row, so one of the two is there from submit until
/// the result expires, and the look at where it stands that follows can only find it
/// further along.
fn stored_ticket(core: &Core<KbfAdapter>, operation: OperationId) -> Option<Ticket> {
    let key = core.with(|_, adapter| adapter.stored_action(operation))?;
    let progress = match core.progress(operation) {
        Ok(Some(progress)) if !progress.is_gone() => progress,
        _ => return None,
    };
    let stage = match progress {
        Progress::Ended(row) => Stage::Done(core.with(|_, adapter| adapter.readable(&row))?),
        Progress::Running => Stage::Executing,
        Progress::Queued | Progress::Gone => Stage::Queued,
    };
    let stage = core.with(|_, adapter| adapter.watch_operation(operation, &stage));
    Some(Ticket {
        name: stored_name(operation),
        instance: key.instance,
        action: key.action,
        stage,
    })
}

/// The `ResultAck` of a refused `Result` of `lease`.
fn refusal(lease: worker::LeaseId, reason: ResultAckReason) -> ResultAck {
    ResultAck {
        lease_id: Some(lease),
        accepted: false,
        reason: reason as i32,
    }
}

/// The wire's name of the reason the core refused a `Result`.
const fn acked(reason: Refused) -> ResultAckReason {
    match reason {
        Refused::Duplicate => ResultAckReason::Duplicate,
        Refused::Stale => ResultAckReason::Stale,
        Refused::UnknownLease => ResultAckReason::UnknownLease,
    }
}

/// A `Result` from `worker`: accepted only if `worker` holds the operation's current
/// lease, and the `Result` names no action but the one that lease runs; an accepted OK
/// result is written to the action cache (unless the action is `do_not_cache` or exited
/// non-zero) before its callers are answered. Returns the acknowledgement, or `None` for a
/// `Result` without a lease id, and for one the state store has not taken yet or the core
/// dropped over its cap: a daemon forgets a `Result` on its ack, so none is sent for a
/// record that is not durable. A held result is acknowledged when its record is written
/// (the adapter's [`settle`](js_server_core::Adapter::settle)).
///
/// # Errors
/// The core's admission refuses the host.
pub async fn report(
    core: &Core<KbfAdapter>,
    cache: &Backend,
    wall: &Wall,
    now: &(dyn Fn() -> FarmTime + Sync),
    worker: &WorkerId,
    result: worker::Result,
) -> Result<Option<ResultAck>, Denied> {
    report_noting(core, cache, wall, now, worker, result, &mut None).await
}

/// How a run that reported ended, for what the farm learns from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ran {
    /// The action the lease ran.
    pub(crate) action: ActionKey,
    /// Whether the run ended on its own (it completed, with any exit code): not killed, lost,
    /// timed out or stored badly.
    pub(crate) clean: bool,
}

/// [`report`], leaving in `ran` what the lease ran and how it ended when the core accepted the
/// `Result` now (not for a refused one, nor one whose record the store has not taken yet).
///
/// # Errors
/// The core's admission refuses the host.
pub(crate) async fn report_noting(
    core: &Core<KbfAdapter>,
    cache: &Backend,
    wall: &Wall,
    now: &(dyn Fn() -> FarmTime + Sync),
    worker: &WorkerId,
    result: worker::Result,
    ran: &mut Option<Ran>,
) -> Result<Option<ResultAck>, Denied> {
    let Some(wire) = result.lease_id else {
        return Ok(None);
    };
    let lease = LeaseId::new(wire.term, wire.seq);
    let operation = match core.standing(worker, lease)? {
        Standing::Current(operation) => operation,
        // The core owes the sender the acknowledgement of the first copy.
        Standing::Held => {
            tracing::info!(%worker, %lease, "result held: its record is not written yet; no ack");
            return Ok(None);
        }
        Standing::Refused(reason) => return Ok(Some(refusal(wire, acked(reason)))),
    };
    // The lease id is this process's, so the run it reports was started for this
    // operation; a daemon that says it ran another action is refused all the same.
    let (sent, recorded, keyed) = core.with(|view, adapter| {
        (
            adapter.sent(view, operation, lease),
            adapter.recording.is_recorded(operation),
            adapter
                .first_waiter(view.sched(), operation)
                .map(|(_, waiter)| waiter.key.clone()),
        )
    });
    let Some(Sent {
        action,
        stamp,
        booked,
        ..
    }) = sent
    else {
        return Ok(Some(refusal(wire, ResultAckReason::UnknownLease)));
    };
    if let Some(named) = &result.action_digest
        && *named != kbf_front::digest_to_proto(&action)
    {
        tracing::warn!(%worker, %lease, "result refused: it names another action");
        return Ok(Some(refusal(wire, ResultAckReason::Mismatch)));
    }
    let read = || wall();
    let ran_on = (worker, stamp);
    // Read before `outcome`, which drops a failed run's result. What the report measured
    // is kept for the hooks around the feed (`action usage:` line, the invocation record).
    let measured = Measured::of(&result).map(|m| m.booked(booked));
    let run = recorded.then(|| {
        let usage = measured.as_ref().map(Measured::usage);
        run_record::of(&result, usage, stamp, read(), booked)
    });
    let (outcome, detail) = answer::outcome(cache, read, lease, ran_on, result).await;
    let clean = matches!(outcome, Outcome::Completed { .. });
    let report = Report {
        operation,
        lease,
        outcome,
    };
    let park = |adapter: &mut KbfAdapter| {
        if let Some(detail) = detail {
            adapter.park(lease, detail);
        }
        adapter.stage_report(measured, run);
    };
    let taken = core.report(now(), worker, report, park).await;
    // None: the ack is sent when the store has taken the record, or never.
    let (accepted, reason) = match taken {
        Taken::Accepted => {
            *ran = keyed.map(|action| Ran { action, clean });
            (true, ResultAckReason::Accepted)
        }
        Taken::Refused(reason) => (false, acked(reason)),
        Taken::Held | Taken::Dropped => return Ok(None),
    };
    Ok(Some(ResultAck {
        lease_id: Some(wire_lease(lease)),
        accepted,
        reason: reason as i32,
    }))
}

impl KbfAdapter {
    /// What was sent for `lease`, which `operation` holds: the action its `Start` named and
    /// the server's times for the run. A lease taken up from the state store was never
    /// started here ([`js_server_core::Adapter::adopted`] is not told it exists), so its
    /// action is its first caller's, and both its times are when that caller was taken up.
    #[must_use]
    pub fn sent(&self, view: &View<'_>, operation: OperationId, lease: LeaseId) -> Option<Sent> {
        self.machine.sent(lease).or_else(|| {
            let (_, waiter) = self.first_waiter(view.sched(), operation)?;
            Some(Sent {
                operation,
                action: waiter.key.action,
                stamp: Stamp {
                    queued: waiter.queued,
                    started: waiter.queued,
                },
                booked: None,
            })
        })
    }

    /// The action `operation` runs or ran, as the state store holds it: in its `op/` row
    /// while it is unfinished, in its `doneof/` row after.
    pub(super) fn stored_action(&self, operation: OperationId) -> Option<ActionKey> {
        if let Some((_, key)) = self.stored_request(operation) {
            return Some(key);
        }
        match KbfExt.ended_parcel(&*self.store, operation) {
            Ok(row) => row.and_then(|row| named::decode(&row).ok()),
            Err(error) => {
                tracing::error!(%operation, %error, "the state store cannot be read");
                None
            }
        }
    }
}
