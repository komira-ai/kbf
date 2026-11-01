//! Tests of the farm's lifecycle on the js core (`lifecycle`). Their farms name the js core,
//! so they run the same with and without `farm-core-js`. The farm suites cover the rest on
//! the core they are built for.

use std::sync::Arc;
use std::time::Duration;

use kbf_front::Cache;
use kbf_sched::{FINISHED_RETENTION, UNSERVABLE_WAIT};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use kbf_types::WorkerId;

use crate::farm::{EpochPolicy, Farm, NodeAction};
use crate::state_store::StateStore;

fn bare_farm() -> Farm {
    Farm::on_core(
        crate::SchedulerCore::Js,
        Arc::new(Cache::memory()).into(),
        UNSERVABLE_WAIT,
        FINISHED_RETENTION,
    )
}

fn farm_over_a_store() -> Farm {
    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    kbf_state::share(&*store).expect("the shared marker");
    bare_farm()
        .with_state(Some(StateStore {
            store,
            epoch: EpochPolicy::Keep,
            members: Default::default(),
            history: None,
        }))
        .expect("a farm over the store")
}

/// Catches an intake poll that does not tell those waiting for it (a test, the frontend's
/// handle) when it ends, a count sent before the poll has run, and a farm with no state store
/// that counts polls the core does not count.
#[tokio::test(start_paused = true)]
async fn each_intake_poll_of_a_farm_over_a_store_is_told_to_those_who_wait_for_it() {
    let farm = farm_over_a_store();
    let mut polls = farm.intake_polls();
    assert_eq!(*polls.borrow_and_update(), 0, "no poll has run");

    farm.intake();
    assert!(polls.has_changed().expect("the farm is alive"));
    assert_eq!(*polls.borrow_and_update(), 1);

    farm.intake();
    polls.changed().await.expect("the farm is alive");
    assert_eq!(*polls.borrow_and_update(), 2);

    let bare = bare_farm();
    let mut polls = bare.intake_polls();
    bare.intake();
    assert!(!polls.has_changed().expect("the farm is alive"));
    assert_eq!(*polls.borrow_and_update(), 0);
}

/// Catches the core's refusal of a node no frontend holds reaching the caller as another
/// error or another text (the operator API answers 404 on it).
#[tokio::test(start_paused = true)]
async fn an_action_on_a_node_no_frontend_holds_is_refused_as_unknown() {
    let farm = farm_over_a_store();
    for action in [
        NodeAction::Cordon,
        NodeAction::Drain(Duration::from_secs(1)),
        NodeAction::Uncordon,
    ] {
        let refused = farm
            .place(&WorkerId::new("nobody"), action)
            .expect_err("no frontend holds the node");
        assert_eq!(refused.to_string(), "no node nobody has registered");
    }
}

/// Catches a flush with nothing to write that waits out its deadline or sleeps once before
/// it looks.
#[tokio::test(start_paused = true)]
async fn a_flush_with_nothing_unsettled_returns_at_once() {
    let farm = farm_over_a_store();
    let before = tokio::time::Instant::now();
    assert!(
        farm.flush_unsettled(Duration::from_secs(30))
            .await
            .is_empty()
    );
    assert_eq!(tokio::time::Instant::now(), before, "it did not wait");
}
