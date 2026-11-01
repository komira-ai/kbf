//! Tests of the count of failed operations the adapter keeps (`kbf_execute_failed_actions_total`).

use js_server_core::Beat;
use kbf_proto::google::rpc;
use kbf_proto::worker;
use kbf_types::{FarmTime, WorkerId};
use tonic::Code;

use super::dispatch;
use super::testing::{Rig, Wire, ran, register, request_of, rig, starts, submit, wall};
use crate::action_usage::Shapes;
use crate::farm::failed_actions::{Class, FailedActions};

fn at(millis: u64) -> FarmTime {
    FarmTime::from_millis(millis)
}

fn failed(rig: &Rig) -> FailedActions {
    rig.core.with(|_, a| a.failed_actions())
}

/// Reports `result` from node-a.
async fn report(rig: &Rig, result: worker::Result) {
    dispatch::report(
        &rig.core,
        &rig.backend,
        &wall(),
        &|| at(0),
        &WorkerId::new("node-a"),
        result,
    )
    .await
    .expect("heard");
}

/// The `Result` of `lease` that ended with `code` and no action result.
fn ended(lease: kbf_types::LeaseId, code: Code) -> worker::Result {
    worker::Result {
        status: Some(rpc::Status {
            code: code as i32,
            message: String::new(),
            details: Vec::new(),
        }),
        action_result: None,
        ..ran(lease, 0)
    }
}

/// Asserts that no class is counted before anything finishes.
#[test]
fn nothing_is_counted_before_an_operation_fails() {
    assert_eq!(failed(&rig()), FailedActions::default());
}

/// Asserts the class of each way a lease can end through a report, by what the callers are
/// told: an exit of 0 counts nothing, a non-zero exit and a timeout are the action's, a
/// result the daemon found invalid is the request's. Each operation is counted once, in its
/// own class, and the others stay at their counts.
#[tokio::test(start_paused = true)]
async fn a_reported_result_is_counted_in_the_class_its_callers_are_told() {
    let rig = rig();
    let mut node = register(&rig, "node-a", 0);
    for tag in 1..=4 {
        submit(&rig, tag, 0).expect("queued");
    }
    let leases = starts(&node.sent());
    assert_eq!(leases.len(), 4);

    report(&rig, ran(leases[0], 0)).await;
    assert_eq!(failed(&rig), FailedActions::default());
    report(&rig, ran(leases[1], 3)).await;
    assert_eq!(failed(&rig).of(Class::Action), 1);
    report(&rig, ended(leases[2], Code::DeadlineExceeded)).await;
    assert_eq!(failed(&rig).of(Class::Action), 2);
    report(&rig, ended(leases[3], Code::InvalidArgument)).await;

    let failed = failed(&rig);
    assert_eq!(failed.of(Class::Action), 2);
    assert_eq!(failed.of(Class::Request), 1);
    assert_eq!(failed.of(Class::Farm), 0);
    assert_eq!(failed.of(Class::Ambiguous), 0);
}

/// Asserts that an operation the core refuses (no live node could run it for the unservable
/// wait) is counted once, as the farm's.
#[tokio::test(start_paused = true)]
async fn a_refused_operation_is_counted_once_as_the_farms() {
    let rig = rig();
    let node = register(&rig, "node-a", 0);
    let mut unservable = super::testing::submission(2);
    unservable.request = request_of(2, 1, 64);
    dispatch::submit(&rig.core, at(0), &Shapes::default(), unservable).expect("queued");

    beat_past_the_wait(&rig, &node).await;

    let failed = failed(&rig);
    assert_eq!(failed.of(Class::Farm), 1);
    assert_eq!(failed.of(Class::Action), 0);
    assert_eq!(failed.of(Class::Request), 0);
}

/// Heartbeats `node` through the wait after which the core refuses work no node can run.
async fn beat_past_the_wait(rig: &Rig, node: &Wire) {
    let worker = WorkerId::new("node-a");
    for (seq, t) in [
        (1, 50),
        (2, 100),
        (3, 150),
        (4, 200),
        (5, 250),
        (6, 299),
        (7, 301),
    ] {
        let now = at(t * 1_000);
        let beat = Beat::new(seq, Vec::new());
        let beat = rig.core.heartbeat(now, &worker, node.stream, beat).await;
        beat.expect("heard");
        rig.core.tick(now).await;
    }
}
