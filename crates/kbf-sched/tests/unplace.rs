//! Undoing a placement ([`Scheduler::unplace`]): a scheduler that loses the claim on an
//! operation to another scheduler of the same state store puts it back in the queue
//! without recording a give-up.

use kbf_caps::NodeCaps;
use kbf_sched::{DaemonInstance, Event, Input, OpState, Request, Scheduler};
use kbf_types::{
    ActionKey, ControlRecord, Digest, DigestFunction, Effect, FarmTime, LeaseGrant, LeaseId,
    LeaseKind, OperationId, Outcome, Qos, Resources, ResultRecord, StateMachine, WaiterId,
    WorkerId,
};

const GIB: u64 = 1 << 30;
const TERM: u64 = 4;

fn request(tag: u8) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
        },
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, GIB),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
    }
}

fn node() -> WorkerId {
    WorkerId::new("node-a")
}

fn feed(s: &mut Scheduler, event: Event) -> Vec<Effect> {
    s.apply(Input::new(FarmTime::default(), event))
}

/// A scheduler of term `TERM` that records its give-ups, with one worker and one queued
/// operation.
fn with_one_operation() -> Scheduler {
    let mut s = Scheduler::new(TERM).recording_requeues();
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    let up = Event::WorkerUp {
        worker: node(),
        instance: DaemonInstance::new("daemon"),
        predecessor_stopped: false,
        capacity: Resources::new(4_000, 8 * GIB),
        caps,
    };
    assert!(feed(&mut s, up).is_empty());
    let submit = Event::Submit {
        waiter: WaiterId(1),
        request: request(1),
    };
    assert!(feed(&mut s, submit).is_empty());
    s
}

/// The one grant a tick proposes.
fn placed(s: &mut Scheduler) -> LeaseGrant {
    let effects = feed(s, Event::Tick);
    let [Effect::Commit(ControlRecord::Lease(grant))] = effects.as_slice() else {
        panic!("{effects:?}");
    };
    grant.clone()
}

/// Commits `grant` and returns the effects.
fn commit(s: &mut Scheduler, grant: &LeaseGrant) -> Vec<Effect> {
    feed(s, Event::Committed(ControlRecord::Lease(grant.clone())))
}

/// Asserts that the operation of an uncommitted grant goes back to the queue with its
/// worker's booking released, that nothing is recorded as given up, and that the grant,
/// committed late, starts nothing.
#[test]
fn an_uncommitted_grant_is_undone_without_a_trace() {
    let mut s = with_one_operation();
    let grant = placed(&mut s);
    assert_eq!(s.booked(&node()), Some(Resources::new(1_000, GIB)));
    assert_eq!(s.leases_held(), 1);

    let back = s.unplace(grant.lease);

    assert_eq!(back, Some(grant.operation));
    assert_eq!(s.state(grant.operation), Some(&OpState::Queued));
    assert_eq!(s.booked(&node()), Some(Resources::default()));
    assert_eq!(s.leases_held(), 0);
    assert_eq!(s.queued_len(), 1);
    assert!(s.take_requeues().is_empty(), "a give-up was recorded");
    assert_eq!(s.losses(grant.operation), Some(0));
    assert!(
        commit(&mut s, &grant).is_empty(),
        "a grant undone before its commit started a run"
    );
}

/// Asserts that a grant whose `Start` was emitted is undone as well, that the lease is
/// then one the caller must cancel on the worker, that a result proposed or committed for
/// it is dropped, and that the next placement numbers its lease past it.
#[test]
fn a_committed_grant_is_undone_and_its_result_is_dropped() {
    let mut s = with_one_operation();
    let grant = placed(&mut s);
    let effects = commit(&mut s, &grant);
    assert!(
        matches!(effects.as_slice(), [Effect::Start(_)]),
        "{effects:?}"
    );

    assert_eq!(s.unplace(grant.lease), Some(grant.operation));

    let running = [grant.lease];
    let to_cancel: Vec<LeaseId> = s.not_held(&node(), &running).collect();
    assert_eq!(to_cancel, [grant.lease]);
    let report = Event::Report {
        operation: grant.operation,
        lease: grant.lease,
        outcome: Outcome::Failed(kbf_types::Failure::Invalid),
    };
    assert!(
        feed(&mut s, report).is_empty(),
        "a report of an undone lease proposed a result"
    );
    let result = ResultRecord {
        lease: grant.lease,
        operation: grant.operation,
        outcome: Outcome::Failed(kbf_types::Failure::Invalid),
    };
    let late = feed(&mut s, Event::Committed(ControlRecord::Result(result)));
    assert!(late.is_empty(), "a result of an undone lease finished it");
    assert_eq!(s.state(grant.operation), Some(&OpState::Queued));

    let next = placed(&mut s);
    assert_eq!(next.operation, grant.operation);
    assert_eq!(next.lease, LeaseId::new(TERM, grant.lease.seq + 1));
}

/// Asserts that a lease whose run has started is not undone: the worker runs it, so the
/// operation stays running and holds its booking.
#[test]
fn a_started_run_is_not_undone() {
    let mut s = with_one_operation();
    let grant = placed(&mut s);
    commit(&mut s, &grant);
    let started = Event::Started {
        operation: grant.operation,
        lease: grant.lease,
    };
    assert!(feed(&mut s, started).is_empty());

    assert_eq!(s.unplace(grant.lease), None);

    assert!(matches!(
        s.state(grant.operation),
        Some(OpState::Running { .. })
    ));
    assert_eq!(s.booked(&node()), Some(Resources::new(1_000, GIB)));
    assert_eq!(s.leases_held(), 1);
}

/// Asserts that a lease the scheduler does not hold is not undone: one never granted, and
/// one already undone.
#[test]
fn a_lease_the_scheduler_does_not_hold_is_not_undone() {
    let mut s = with_one_operation();
    assert_eq!(s.unplace(LeaseId::new(TERM, 99)), None);
    let grant = placed(&mut s);
    assert_eq!(s.unplace(grant.lease), Some(grant.operation));

    assert_eq!(s.unplace(grant.lease), None);

    assert_eq!(s.state(OperationId(99)), None);
    assert_eq!(s.queued_len(), 1);
}
