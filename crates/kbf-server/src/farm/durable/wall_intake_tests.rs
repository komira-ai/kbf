//! The wall time of a request another frontend of the state store queued, taken in at an
//! intake poll. Time is tokio's paused clock, which the test moves.

use std::sync::Arc;
use std::time::Duration;

use kbf_sched::WholeState;
use kbf_state::ManualClock;
use kbf_state::fake::{Fake, Flaw};
use kbf_types::{LeaseKind, OperationId};

use super::*;
use crate::farm::whole_node_tests::Rig;

/// Catches the wall time of a request another frontend of the store queued being left out
/// when this one takes it in at an intake poll: the hold would never be given up.
#[tokio::test(start_paused = true)]
async fn a_request_another_frontend_queued_comes_in_with_its_wall_time() {
    let store = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let mut rig = Rig::over(
        &[
            "--whole-node-max=1",
            "--whole-node-keep-for-ordinary=0",
            "--whole-node-drain-after-secs=10",
        ],
        &["node-a"],
        &store,
    );
    let other = Durable::open(store.clone(), EpochPolicy::Keep, 1)
        .expect("open")
        .0;
    let queue = |id: u64, tag: u8, kind: LeaseKind, wall: Option<Duration>| {
        let request = kbf_sched::Request {
            key: kbf_types::ActionKey {
                instance: "main".to_owned(),
                action: kbf_types::Digest::new(kbf_types::DigestFunction::Sha256, [tag; 32], 1),
            }
            .into(),
            qos: kbf_types::Qos::Ci,
            kind,
            resources: kbf_types::Resources::new(1_000, 1 << 30),
            hermetic: true,
            do_not_cache: false,
            needs: kbf_caps::Request::default(),
            whole_worker: false,
        };
        let subject = crate::action_usage::Subject::of(Default::default()).0;
        let id = other.reserve_operation(OperationId(id)).expect("an id");
        other
            .submit_keyed(id, &request, &subject, None, wall)
            .expect("submit");
        id
    };
    queue(5000, 10, LeaseKind::Action, None);
    let whole = queue(
        5001,
        1,
        LeaseKind::WholeMachine,
        Some(Duration::from_secs(100)),
    );
    rig.farm.intake();
    rig.pass(Duration::from_secs(10)).await;
    let Some(WholeState::Holding {
        made,
        deadline: Some(deadline),
        ..
    }) = rig.whole_state(whole)
    else {
        panic!("not holding: {:?}", rig.whole_state(whole));
    };
    assert_eq!(deadline.as_millis(), made.as_millis() + 100_000);
}
