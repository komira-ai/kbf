//! Taking back a refusal for want of a worker ([`Scheduler::hold_refusal`]), and the test
//! of whether a worker could run a request ([`Request::runnable_on`]) that a caller applies
//! to the workers of other schedulers sharing its state store.

use std::time::Duration;

use kbf_caps::NodeCaps;
use kbf_sched::{DaemonInstance, Event, Input, OpState, Request, Scheduler};
use kbf_types::{
    ActionKey, ControlRecord, Digest, DigestFunction, Effect, FarmTime, LeaseKind, OperationId,
    Qos, Refusal, RefusalRecord, Resources, StateMachine, WaiterId, WorkerId,
};

const GIB: u64 = 1 << 30;
/// The unservable wait these tests run with.
const WAIT: Duration = Duration::from_secs(30);

/// Action `tag`, one core and 1 GiB, for a worker of operating system `os`.
fn request(tag: u8, os: &str) -> Request {
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
        needs: kbf_caps::Request::from_platform([("OSFamily", os)]).unwrap(),
    }
}

fn linux() -> NodeCaps {
    NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .unwrap()
        .with_drivers(["container"])
}

fn mac() -> NodeCaps {
    NodeCaps::from_report([("arch", "arm64"), ("os", "macos")])
        .unwrap()
        .with_drivers(["native"])
}

fn at(secs: u64) -> FarmTime {
    FarmTime::from_millis(secs * 1_000)
}

fn feed(s: &mut Scheduler, secs: u64, event: Event) -> Vec<Effect> {
    s.apply(Input::new(at(secs), event))
}

/// The answer of a caller that must not be asked about the workers elsewhere.
fn never(_: &Request) -> bool {
    panic!("asked about the workers elsewhere")
}

/// A scheduler with the short unservable wait and one Linux worker, named `linux`.
fn with_a_linux_worker() -> Scheduler {
    let mut s = Scheduler::new(1).with_unservable_wait(WAIT);
    let up = Event::WorkerUp {
        worker: WorkerId::new("linux"),
        instance: DaemonInstance::new("linux"),
        predecessor_stopped: false,
        capacity: Resources::new(4_000, 8 * GIB),
        caps: linux(),
    };
    assert!(feed(&mut s, 0, up).is_empty());
    s
}

/// Submits the action `tag` for `os` to `s` as caller 1.
fn submit(s: &mut Scheduler, tag: u8, os: &str) {
    let submit = Event::Submit {
        waiter: WaiterId(1),
        request: request(tag, os),
    };
    assert!(feed(s, 0, submit).is_empty());
}

/// Submits a macOS action to `s` and returns the refusal it proposes once the wait is up.
fn proposed_refusal(s: &mut Scheduler) -> RefusalRecord {
    submit(s, 1, "macos");
    let waiting = feed(s, 0, Event::Tick);
    let [Effect::Waiting(_)] = waiting.as_slice() else {
        panic!("{waiting:?}");
    };
    let proposed = feed(s, WAIT.as_secs(), Event::Tick);
    let [Effect::Commit(ControlRecord::Refusal(record))] = proposed.as_slice() else {
        panic!("{proposed:?}");
    };
    record.clone()
}

/// Asserts that a refusal proposed for want of a worker is taken back when a worker the
/// scheduler does not hold could run the operation: the operation is queued again, still
/// waiting with its reason, the callers are not answered, and it is proposed for refusal
/// again only after a whole unservable wait from the moment it was held, not at once.
#[test]
fn a_refusal_is_taken_back_when_a_worker_elsewhere_could_run_the_operation() {
    let mut s = with_a_linux_worker();
    let record = proposed_refusal(&mut s);
    assert_eq!(s.queued_len(), 0, "a proposed refusal leaves the queue");
    let elsewhere = Resources::new(4_000, 8 * GIB);

    let held = s.hold_refusal(record.operation, |r| r.runnable_on(&mac(), &elsewhere));

    assert!(held);
    assert_eq!(s.state(record.operation), Some(&OpState::Queued));
    assert_eq!(s.queued_len(), 1);
    assert!(s.waiting(record.operation).is_some());
    assert!(feed(&mut s, WAIT.as_secs(), Event::Tick).is_empty());
    let (before, due) = (2 * WAIT.as_secs() - 1, 2 * WAIT.as_secs());
    let beat = Event::Heartbeat {
        worker: WorkerId::new("linux"),
        running: Vec::new(),
    };
    drop(feed(&mut s, before, beat));
    assert!(feed(&mut s, before, Event::Tick).is_empty());
    let again = feed(&mut s, due, Event::Tick);
    let [Effect::Commit(ControlRecord::Refusal(second))] = again.as_slice() else {
        panic!("{again:?}");
    };
    assert_eq!(second.operation, record.operation);
}

/// Asserts that a refusal stands when no worker elsewhere could run the operation: nothing
/// is put back in the queue, and the refusal, committed, answers the callers.
#[test]
fn a_refusal_stands_when_no_worker_elsewhere_could_run_the_operation() {
    let mut s = with_a_linux_worker();
    let record = proposed_refusal(&mut s);

    let held = s.hold_refusal(record.operation, |_| false);

    assert!(!held);
    assert_eq!(s.queued_len(), 0);
    let committed = feed(
        &mut s,
        WAIT.as_secs(),
        Event::Committed(ControlRecord::Refusal(record.clone())),
    );
    assert_eq!(
        committed,
        [Effect::Refuse(Refusal {
            operation: record.operation,
            waiters: vec![WaiterId(1)],
            reason: record.reason,
        })]
    );
}

/// Asserts that an operation the scheduler does not hold, and one that is not queued (it
/// holds a lease), are not touched: the question about workers elsewhere is not even asked.
#[test]
fn an_operation_that_is_unknown_or_not_queued_is_not_held() {
    let mut s = with_a_linux_worker();
    assert!(!s.hold_refusal(OperationId(9), never));
    submit(&mut s, 1, "linux");
    let placed = feed(&mut s, 0, Event::Tick);
    let [Effect::Commit(ControlRecord::Lease(_))] = placed.as_slice() else {
        panic!("{placed:?}");
    };

    let held = s.hold_refusal(OperationId(0), never);

    assert!(!held);
    assert_eq!(s.queued_len(), 0, "a leased operation was queued again");
}

/// Asserts that a refusal for lost leases is not taken back, whatever workers there are
/// elsewhere: they do not change how many times the operation lost its lease. A scheduler
/// that refuses at the first lost lease, here before any, proposes it for the operation at
/// once.
#[test]
fn a_refusal_for_lost_leases_is_not_taken_back() {
    let mut s = with_a_linux_worker().with_loss_requeues(0);
    submit(&mut s, 1, "linux");
    let proposed = feed(&mut s, 0, Event::Tick);
    let [Effect::Commit(ControlRecord::Refusal(_))] = proposed.as_slice() else {
        panic!("{proposed:?}");
    };

    let held = s.hold_refusal(OperationId(0), never);

    assert!(!held);
    assert_eq!(s.queued_len(), 0);
}

/// Asserts that holding an operation that never waited returns true and leaves it queued
/// with no waiting reason.
#[test]
fn an_operation_that_never_waited_stays_queued_once() {
    let mut s = with_a_linux_worker();
    submit(&mut s, 1, "linux");
    assert_eq!(s.waiting(OperationId(0)), None);

    let held = s.hold_refusal(OperationId(0), |_| true);

    assert!(held);
    assert_eq!(s.queued_len(), 1);
    assert_eq!(s.waiting(OperationId(0)), None);
}

/// Asserts that a worker could run a request only if it reports a driver that serves the
/// lease kind, its capabilities satisfy the platform and its whole capacity holds the
/// resources, each of the three on its own.
#[test]
fn a_worker_runs_a_request_only_if_driver_platform_and_capacity_all_fit() {
    let linux_action = request(1, "linux");
    let big = Resources::new(4_000, 8 * GIB);
    let no_driver = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")]).unwrap();

    assert!(linux_action.runnable_on(&linux(), &big));
    assert!(
        !linux_action.runnable_on(&no_driver, &big),
        "a worker with no driver for the lease kind"
    );
    assert!(
        !linux_action.runnable_on(&mac(), &big),
        "a worker whose platform is not the action's"
    );
    assert!(
        !linux_action.runnable_on(&linux(), &Resources::new(500, GIB)),
        "a worker smaller than the request"
    );
}
