//! The periodic tick of a farm (issue #22): a lease given up for silence past its
//! operation's farm budget finishes the operation, and its callers are told, though
//! no report or heartbeat is what found it.

use std::sync::Arc;
use std::time::Duration;

use kbf_caps::NodeCaps;
use kbf_front::{Cache, Dispatch, Finished, Stage, Submission};
use kbf_proto::worker::{ServerMessage, server_message};
use kbf_sched::fence::{LEASE_GRACE, START_GRACE};
use kbf_sched::{DaemonInstance, FARM_RERUNS, NOT_STARTED_RERUNS, Request};
use kbf_server::Farm;
use kbf_server::api::NO_DRAIN_DEADLINE;
use kbf_server::farm::{NodeAction, Reported, StreamId};
use kbf_server::fleet::PlacementView;
use kbf_types::{ActionKey, Digest, DigestFunction, LeaseId, LeaseKind, Qos, Resources, WorkerId};
use tokio::sync::mpsc;

type Responses = mpsc::UnboundedReceiver<Result<ServerMessage, tonic::Status>>;

const NODES: [&str; 3] = ["node-a", "node-b", "node-c"];

fn register(farm: &Farm, node: &str) -> (StreamId, Responses) {
    let (outbound, responses) = mpsc::unbounded_channel();
    let caps = NodeCaps::from_report([("arch", "x86_64")])
        .expect("caps")
        .with_drivers(["container"]);
    let stream = farm.register(
        &WorkerId::new(node),
        DaemonInstance::new(node),
        DaemonInstance::default(),
        Reported::serving(Resources::new(8_000, 16 << 30), caps),
        outbound,
        ServerMessage::default(),
    );
    (stream, responses)
}

/// The nodes (by index) that have been sent a `Start` since the last call.
fn started(responses: &mut [Responses]) -> Vec<usize> {
    let mut nodes = Vec::new();
    for (i, rx) in responses.iter_mut().enumerate() {
        while let Ok(message) = rx.try_recv() {
            if matches!(
                message.expect("a message").message,
                Some(server_message::Message::Start(_))
            ) {
                nodes.push(i);
            }
        }
    }
    nodes
}

/// Catches a tick that drops the answer of an operation it finished (the farm's
/// `tick` once fed the scheduler through the quiet path: debug builds panicked, and
/// release builds finished the operation with nobody told): three nodes each go
/// silent holding the lease in turn, with no report and no heartbeat from it, and the
/// third silence is past the budget. The caller's ticket ends INTERNAL, naming the
/// node and the three runs.
#[tokio::test(start_paused = true)]
async fn a_silent_node_past_the_budget_answers_the_callers_on_a_tick() {
    let farm = Farm::new(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    );
    let (mut streams, mut responses) = (Vec::new(), Vec::new());
    for node in NODES {
        let (stream, rx) = register(&farm, node);
        streams.push(stream);
        responses.push(rx);
    }
    let request = Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: Digest::new(DigestFunction::Sha256, [7; 32], 7),
        }
        .into(),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        needs: kbf_caps::Request::default(),
        hermetic: true,
        whole_worker: false,
        do_not_cache: false,
    };
    let ticket = farm
        .submit(Submission {
            request,
            call: None,
            described: kbf_front::Described::default(),
            evidence: Default::default(),
            max_wall: None,
        })
        .expect("queued");
    let mut seq = 0;
    let mut holders = started(&mut responses);
    for _ in 0..=FARM_RERUNS {
        let [holder] = holders[..] else {
            panic!("want one holder, got {holders:?}");
        };
        assert!(
            !matches!(*ticket.stage.borrow(), Stage::Done(_)),
            "answered before the budget was used"
        );
        // The holder is heard now, and then not again.
        seq += 1;
        assert!(
            farm.heartbeat(
                &WorkerId::new(NODES[holder]),
                streams[holder],
                seq,
                Vec::new()
            )
            .await
        );
        // The others are heard half way, so only the tick finds the holder silent (a
        // heartbeat after the grace would expire the lease itself, and answer).
        tokio::time::advance(LEASE_GRACE / 2).await;
        seq += 1;
        for (i, node) in NODES.iter().enumerate().filter(|(i, _)| *i != holder) {
            assert!(
                farm.heartbeat(&WorkerId::new(*node), streams[i], seq, Vec::new())
                    .await
            );
        }
        tokio::time::advance(LEASE_GRACE / 2 + Duration::from_secs(1)).await;
        farm.tick().await;
        holders = started(&mut responses);
    }
    let Stage::Done(Finished::Failed(status)) = ticket.stage.borrow().clone() else {
        panic!("the callers were not answered: {:?}", ticket.stage.borrow());
    };
    assert_eq!(status.code, tonic::Code::Internal as i32, "{status:?}");
    assert!(
        status.message.starts_with("kbf farm fault on node-"),
        "{status:?}"
    );
    assert!(
        status.message.contains("Runs: 3 (") && status.message.matches("fenced").count() >= 3,
        "{status:?}"
    );
}

/// Catches the callers of an operation the farm never started being told nothing true:
/// a node that heartbeats and drops every `Start` ends the operation after
/// `1 + NOT_STARTED_RERUNS` give-ups, with no farm run to name. The ticket ends
/// INTERNAL and says the farm never started the action (not "on  : , reruns used up"
/// with an empty node), and the Starts sent are exactly those give-ups.
#[tokio::test(start_paused = true)]
async fn a_node_that_drops_every_start_answers_the_callers_that_it_never_started() {
    let farm = Farm::new(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    );
    let (stream, rx) = register(&farm, NODES[0]);
    let mut responses = [rx];
    let request = Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: Digest::new(DigestFunction::Sha256, [8; 32], 8),
        }
        .into(),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        needs: kbf_caps::Request::default(),
        hermetic: true,
        whole_worker: false,
        do_not_cache: false,
    };
    let ticket = farm
        .submit(Submission {
            request,
            call: None,
            described: kbf_front::Described::default(),
            evidence: Default::default(),
            max_wall: None,
        })
        .expect("queued");
    let mut starts = 0;
    for seq in 1..=u64::from(NOT_STARTED_RERUNS) + 1 {
        farm.tick().await;
        starts += started(&mut responses).len();
        tokio::time::advance(START_GRACE + Duration::from_secs(1)).await;
        assert!(
            farm.heartbeat(&WorkerId::new(NODES[0]), stream, seq, Vec::new())
                .await
        );
    }
    assert_eq!(starts, usize::try_from(NOT_STARTED_RERUNS).unwrap() + 1);
    let stage = ticket.stage.borrow().clone();
    assert!(
        matches!(&stage, Stage::Done(Finished::Failed(s))
            if s.code == tonic::Code::Internal as i32 && s.message.contains("never started the action")),
        "{stage:?}"
    );
}

/// A farm with one registered node that holds one running lease.
fn farm_with_a_lease() -> (Farm, StreamId, WorkerId, String) {
    let farm = Farm::new(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    );
    let (stream, rx) = register(&farm, NODES[0]);
    let mut responses = [rx];
    let request = Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: Digest::new(DigestFunction::Sha256, [9; 32], 9),
        }
        .into(),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        needs: kbf_caps::Request::default(),
        hermetic: true,
        whole_worker: false,
        do_not_cache: false,
    };
    let _ticket = farm
        .submit(Submission {
            request,
            call: None,
            described: kbf_front::Described::default(),
            evidence: Default::default(),
            max_wall: None,
        })
        .expect("queued");
    assert_eq!(started(&mut responses), [0]);
    let worker = WorkerId::new(NODES[0]);
    let held = farm.node_view(&worker).expect("the node").lease_count;
    assert_eq!(held, 1);
    let lease = farm
        .node_detail(&worker)
        .expect("a read")
        .expect("the node")
        .leases
        .remove(0);
    (farm, stream, worker, lease)
}

/// Catches a tick that does not pause a drain at its deadline (work still running on a
/// draining node would be reported as drained, and a rollout would cut it off), or one
/// that loses the pause once it is set: a node holds a lease, is drained for ten
/// seconds, and goes past the deadline; the tick shows it `DrainPaused` with the lease
/// still on it, and a second tick leaves it so.
#[tokio::test(start_paused = true)]
async fn a_drain_with_a_lease_still_running_pauses_at_its_deadline() {
    let (farm, _stream, worker, _lease) = farm_with_a_lease();
    farm.place(&worker, NodeAction::Drain(Duration::from_secs(10)))
        .expect("known node");
    tokio::time::advance(Duration::from_secs(11)).await;
    for _ in 0..2 {
        farm.tick().await;
        let view = farm.node_view(&worker).expect("the node");
        let PlacementView::DrainPaused { leases, .. } = view.placement else {
            panic!("not paused at the deadline: {:?}", view.placement);
        };
        assert_eq!(leases.len(), 1, "{leases:?}");
    }
}

/// Catches a drain with no deadline that ends at some default one (it was 30 minutes: the
/// node paused, and a rollout that waited on it was left with a node that never reported
/// drained), that shows a deadline (a year in the far future) for it, or that does not end
/// when its lease does: a node whose heartbeats list its lease is drained with no deadline
/// and goes three hours on; it is still `Draining` with the lease and no deadline, never
/// `DrainPaused`, and once it falls silent past the grace the lease is given up and the
/// node is `Drained`.
#[tokio::test(start_paused = true)]
async fn a_drain_with_no_deadline_waits_for_its_leases_however_long() {
    let (farm, stream, worker, lease) = farm_with_a_lease();
    let (term, seq) = lease.split_once('.').expect("term.seq");
    let lease_id = LeaseId::new(term.parse().expect("term"), seq.parse().expect("seq"));
    let placed = farm
        .place(&worker, NodeAction::Drain(NO_DRAIN_DEADLINE))
        .expect("known node");
    assert_eq!(
        placed.placement,
        PlacementView::Draining {
            deadline_unix_ms: None,
            leases: vec![lease.clone()],
        }
    );
    let step = LEASE_GRACE / 2;
    let mut waited = Duration::ZERO;
    let mut beat = 0;
    while waited < Duration::from_secs(3 * 60 * 60) {
        tokio::time::advance(step).await;
        waited += step;
        beat += 1;
        assert!(farm.heartbeat(&worker, stream, beat, vec![lease_id]).await);
        farm.tick().await;
    }
    let view = farm.node_view(&worker).expect("the node");
    assert_eq!(
        view.placement,
        PlacementView::Draining {
            deadline_unix_ms: None,
            leases: vec![lease],
        },
        "still waiting for the lease"
    );
    let shown = serde_json::to_value(&view).expect("JSON");
    assert!(
        shown["placement"].get("deadline_unix_ms").is_none(),
        "{shown}"
    );

    tokio::time::advance(LEASE_GRACE + Duration::from_secs(1)).await;
    farm.tick().await;
    let view = farm.node_view(&worker).expect("the node");
    assert_eq!(view.placement, PlacementView::Drained);
}
