//! A farm whose storage cannot answer while an OK result is reported: the result waits,
//! and the operation neither fails nor spends its farm reruns. The clock is tokio's,
//! paused.

use std::sync::Arc;

use kbf_front::{Backend, Described, Finished, Stage, Submission};
use kbf_storage::Storage;
use kbf_types::{ActionKey, Digest, DigestFunction, LeaseKind, Qos, Resources};

use super::*;
use crate::answer_tests::{Flaky, ok_result};

/// Registers `node-a` with room for four actions.
fn register(farm: &Farm) -> mpsc::UnboundedReceiver<Result<ServerMessage, Status>> {
    let caps = kbf_caps::NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    let (outbound, inbox) = mpsc::unbounded_channel();
    farm.register(
        &WorkerId::new("node-a"),
        DaemonInstance::new("daemon"),
        DaemonInstance::new(""),
        Reported::serving(Resources::new(4_000, 8 << 30), caps),
        outbound,
        ServerMessage::default(),
    );
    inbox
}

fn action() -> Digest {
    Digest::new(DigestFunction::Sha256, [1; 32], 1)
}

/// Submits the action and returns its caller's ticket.
fn submit(farm: &Farm) -> kbf_front::Ticket {
    let request = kbf_sched::Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: action(),
        }
        .into(),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
        whole_worker: false,
    };
    farm.submit(Submission {
        request,
        call: None,
        described: Described::default(),
        evidence: Default::default(),
        max_wall: None,
    })
    .expect("submitted")
}

/// Catches an OK result whose outputs cannot be checked because storage cannot answer
/// being charged to the operation's farm reruns: storage is down for three times as many
/// asks as the farm has reruns, and the one run's result is still accepted, its caller is
/// answered with it, and its action-cache entry is written once storage answers.
#[tokio::test(start_paused = true)]
async fn a_storage_outage_longer_than_the_reruns_does_not_fail_the_operation() {
    let store = Flaky::down_for(3 * (1 + kbf_sched::FARM_RERUNS));
    let farm = Farm::new(
        Backend::remote(Arc::clone(&store) as Arc<dyn Storage>),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    );
    let _inbox = register(&farm);
    let ticket = submit(&farm);
    let lease = {
        let views = farm.lease_views(None);
        let [view] = &views.leases[..] else {
            panic!("one lease expected");
        };
        view.lease
    };

    let ack = farm
        .report(&WorkerId::new("node-a"), ok_result(lease))
        .await
        .expect("acknowledged");

    assert!(ack.accepted, "{ack:?}");
    let stage = ticket.stage.borrow().clone();
    assert!(
        matches!(stage, Stage::Done(Finished::Ran(_))),
        "the caller was not answered with the result: {stage:?}"
    );
    assert!(store.holds(&action()).await, "the entry was not written");
    assert_eq!(
        store.asks(),
        3 * (1 + kbf_sched::FARM_RERUNS) + 1,
        "the result was checked until storage answered, and the action did not run again"
    );
}
