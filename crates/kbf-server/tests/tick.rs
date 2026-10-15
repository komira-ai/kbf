//! The periodic tick of a farm (issue #22): a lease given up for silence past its
//! operation's farm budget finishes the operation, and its callers are told, though
//! no report or heartbeat is what found it.

use std::sync::Arc;
use std::time::Duration;

use kbf_caps::NodeCaps;
use kbf_front::{Cache, Dispatch, Finished, MemoryMetaLog, Stage, Submission};
use kbf_objstore::MemoryStore;
use kbf_proto::worker::{ServerMessage, server_message};
use kbf_sched::fence::{LEASE_GRACE, START_GRACE};
use kbf_sched::{DaemonInstance, FARM_RERUNS, NOT_STARTED_RERUNS, Request};
use kbf_server::Farm;
use kbf_server::farm::{Reported, StreamId};
use kbf_types::{ActionKey, Digest, DigestFunction, LeaseKind, Qos, Resources, WorkerId};
use tokio::sync::mpsc;

type Responses = mpsc::UnboundedReceiver<Result<ServerMessage, tonic::Status>>;

const NODES: [&str; 3] = ["node-a", "node-b", "node-c"];

fn register(farm: &Farm<MemoryMetaLog, MemoryStore>, node: &str) -> (StreamId, Responses) {
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
        },
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        needs: kbf_caps::Request::default(),
        hermetic: true,
        do_not_cache: false,
    };
    let ticket = farm
        .submit(Submission {
            request,
            call: None,
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
        },
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        needs: kbf_caps::Request::default(),
        hermetic: true,
        do_not_cache: false,
    };
    let ticket = farm
        .submit(Submission {
            request,
            call: None,
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
