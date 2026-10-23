//! Forgetting a queued operation another scheduler of the same state store took
//! ([`Scheduler::forget_queued`]), and taking up a floor another scheduler raised
//! ([`Scheduler::raise_floor`], [`Scheduler::raise_key_floor`]).

use kbf_caps::NodeCaps;
use kbf_sched::{DaemonInstance, Event, Floor, Input, KeyFloor, OpState, Request, Scheduler};
use kbf_types::{
    ActionKey, ControlRecord, Digest, DigestFunction, Effect, FarmTime, LeaseKind, OperationId,
    Resources, StateMachine, WaiterId, WorkerId,
};

const GIB: u64 = 1 << 30;
const TERM: u64 = 4;

fn request(tag: u8) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
        },
        qos: kbf_types::Qos::Ci,
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

/// A scheduler of term `TERM` with one worker.
fn with_a_worker() -> Scheduler {
    let mut s = Scheduler::new(TERM);
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
    s
}

fn submit(s: &mut Scheduler, waiter: u64, request: Request) {
    let event = Event::Submit {
        waiter: WaiterId(waiter),
        request,
    };
    assert!(feed(s, event).is_empty());
}

/// Asserts that a queued operation is forgotten with every caller it had, that nothing of
/// it is left to place or to join, and that a request for its action then queues a new
/// operation under the next id.
#[test]
fn a_queued_operation_another_scheduler_took_is_forgotten_with_its_callers() {
    let mut s = with_a_worker();
    submit(&mut s, 1, request(1));
    submit(&mut s, 2, request(1));
    assert_eq!(s.queued_len(), 1);
    assert_eq!(s.twin(&request(1)), Some(OperationId(0)));

    let callers = s.forget_queued(OperationId(0));

    assert_eq!(callers, Some(vec![WaiterId(1), WaiterId(2)]));
    assert_eq!(s.state(OperationId(0)), None);
    assert_eq!((s.queued_len(), s.operations()), (0, 0));
    assert_eq!(s.twin(&request(1)), None);
    assert!(
        feed(&mut s, Event::Tick).is_empty(),
        "a forgotten operation was placed"
    );
    submit(&mut s, 3, request(1));
    assert_eq!(s.state(OperationId(1)), Some(&OpState::Queued));
    assert_eq!(s.twin(&request(1)), Some(OperationId(1)));
}

/// Asserts that an operation no request can join (a networked one) is forgotten too, and
/// that forgetting it leaves a joinable operation of the same action in flight.
#[test]
fn a_queued_operation_that_joins_no_twin_is_forgotten_and_leaves_the_others() {
    let mut s = with_a_worker();
    let networked = Request {
        hermetic: false,
        ..request(1)
    };
    submit(&mut s, 1, networked);
    submit(&mut s, 2, request(1));
    assert_eq!(s.twin(&request(1)), Some(OperationId(1)));

    let callers = s.forget_queued(OperationId(0));

    assert_eq!(callers, Some(vec![WaiterId(1)]));
    assert_eq!(s.twin(&request(1)), Some(OperationId(1)));
    assert_eq!(s.queued_len(), 1);
}

/// Asserts that an operation that is not queued here stays as it is: one that holds a
/// lease, and one the scheduler never saw.
#[test]
fn an_operation_that_is_not_queued_is_not_forgotten() {
    let mut s = with_a_worker();
    submit(&mut s, 1, request(1));
    let placed = feed(&mut s, Event::Tick);
    let [Effect::Commit(ControlRecord::Lease(grant))] = placed.as_slice() else {
        panic!("{placed:?}");
    };

    assert_eq!(s.forget_queued(grant.operation), None);
    assert_eq!(s.forget_queued(OperationId(99)), None);

    assert!(matches!(
        s.state(grant.operation),
        Some(OpState::Leased { .. })
    ));
    assert_eq!(s.leases_held(), 1);
}

fn floor(tag: u8, ask: u64) -> Floor {
    Floor {
        key: request(tag).key,
        ask,
        stamp: 7,
    }
}

/// Asserts that a floor another scheduler raised is taken up when it is more than the one
/// held or none is held, and not otherwise: a floor never falls, and an equal one is no
/// news.
#[test]
fn a_floor_another_scheduler_raised_is_taken_up_when_it_is_higher() {
    let mut s = Scheduler::new(TERM);
    let key = request(1).key;

    assert!(s.raise_floor(&floor(1, 2 * GIB)));
    assert_eq!(s.memory_floor(&key), Some(2 * GIB));
    assert!(!s.raise_floor(&floor(1, 2 * GIB)), "an equal floor rose");
    assert!(!s.raise_floor(&floor(1, GIB)), "a floor fell");
    assert_eq!(s.memory_floor(&key), Some(2 * GIB));
    assert!(s.raise_floor(&floor(1, 4 * GIB)));
    assert_eq!(s.memory_floor(&key), Some(4 * GIB));
    assert_eq!(s.memory_floor(&request(2).key), None);
}

/// Asserts that a key floor another scheduler raised is taken up when it is more than the one
/// held or none is held, and not otherwise.
#[test]
fn a_key_floor_another_scheduler_raised_is_taken_up_when_it_is_higher() {
    let mut s = Scheduler::new(TERM);
    let key_floor = |ask| KeyFloor {
        key: 9,
        ask,
        stamp: 3,
    };

    assert!(s.raise_key_floor(&key_floor(2 * GIB)));
    assert_eq!(s.key_floor(9), Some(2 * GIB));
    assert!(!s.raise_key_floor(&key_floor(2 * GIB)));
    assert!(!s.raise_key_floor(&key_floor(GIB)));
    assert!(s.raise_key_floor(&key_floor(4 * GIB)));
    assert_eq!(s.key_floor(9), Some(4 * GIB));
    assert_eq!(s.key_floor(8), None);
}
