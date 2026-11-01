//! `--whole-node-max` over a state store: the wall time of a queued request is kept with its
//! row, and a farm that takes the request up again gives it to the scheduler before its
//! first placement round, so the hold has the deadline the first farm would have given it.
//! Time is tokio's paused clock, which the tests move.

use std::sync::Arc;
use std::time::Duration;

use kbf_state::fake::{Fake, Flaw};
use kbf_state::{Cond, ManualClock, Store, Txn};
use kbf_types::{LeaseKind, OperationId};

use kbf_sched::WholeState;

use super::whole_node_tests::Rig;

const FLAGS: [&str; 3] = [
    "--whole-node-max=1",
    "--whole-node-keep-for-ordinary=0",
    "--whole-node-drain-after-secs=10",
];

fn store() -> Arc<Fake> {
    Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ))
}

fn wall_rows(store: &Arc<Fake>) -> Vec<String> {
    let rows = store.scan_prefix("opwall/").expect("scan");
    rows.into_iter().map(|(key, _)| key).collect()
}

/// A farm with no nodes queues an action and a whole-machine request with `wall`; `before`
/// then changes the store; a second farm over it registers one node, takes both up and
/// places them: the action leases the node and the request holds it. Returns the time the
/// hold was made and its deadline, in milliseconds.
async fn restarted_hold(
    wall: Option<Duration>,
    before: impl FnOnce(&Arc<Fake>, OperationId),
) -> (u64, Option<u64>) {
    let store = store();
    let whole = {
        let first = Rig::over(&FLAGS, &[], &store);
        first.submit(10, LeaseKind::Action, None);
        first.submit(1, LeaseKind::WholeMachine, wall)
    };
    before(&store, whole);
    let mut second = Rig::over(&FLAGS, &["node-a"], &store);
    second.pass(Duration::from_secs(10)).await;
    let Some(WholeState::Holding { made, deadline, .. }) = second.whole_state(whole) else {
        panic!("not holding: {:?}", second.whole_state(whole));
    };
    (made.as_millis(), deadline.map(|d| d.as_millis()))
}

/// Catches a wall time lost between the first farm and the second (not written with the
/// row, or not given to the scheduler when the row is taken up): the recovered request
/// would hold with the end of time as its deadline and never give the hold up.
#[tokio::test(start_paused = true)]
async fn a_queued_request_keeps_its_wall_time_over_a_restart() {
    let (made, deadline) = restarted_hold(Some(Duration::from_millis(100_500)), |store, whole| {
        assert_eq!(wall_rows(store), [format!("opwall/{:016x}", whole.0)]);
    })
    .await;
    assert_eq!(deadline, Some(made + 100_500));
}

/// Catches a wall time invented for a request that has none: no row is written, and the
/// recovered hold has no deadline of its own.
#[tokio::test(start_paused = true)]
async fn a_queued_request_with_no_wall_time_recovers_with_none() {
    let (_, deadline) = restarted_hold(None, |store, _| {
        assert!(wall_rows(store).is_empty());
    })
    .await;
    assert_eq!(deadline, Some(u64::MAX));
}

/// Catches a store written before the wall rows existed refusing to start or recovering a
/// wall time from nothing: the request's row is there and its wall row is not.
#[tokio::test(start_paused = true)]
async fn a_row_written_without_a_wall_row_recovers_with_none() {
    let (_, deadline) = restarted_hold(Some(Duration::from_secs(100)), |store, _| {
        let rows = wall_rows(store);
        assert_eq!(rows.len(), 1);
        let gone = rows
            .into_iter()
            .fold(Txn::new(), |txn, k| txn.delete(k, Cond::Any));
        store.txn(gone).expect("write").expect("applied");
    })
    .await;
    assert_eq!(deadline, Some(u64::MAX));
}
