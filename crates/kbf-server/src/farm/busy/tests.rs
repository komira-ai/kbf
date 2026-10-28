//! A heartbeat's busy reading reaching the scheduler's admission by measured CPU.

use std::sync::Arc;
use std::time::Duration;

use kbf_front::{Described, Dispatch as _, Submission};
use kbf_proto::worker::ServerMessage;
use kbf_sched::{CpuAdmission, DaemonInstance, Request};
use kbf_types::{ActionKey, Digest, DigestFunction, LeaseKind, Qos, Resources, WorkerId};

use crate::farm::phase::Reported;
use crate::farm::{Farm, StreamId};

/// A farm that admits an action to a node only under 50 percent busy, whose nodes have 4
/// CPUs: a reading of 2000 closes it.
fn farm(admission: Option<CpuAdmission>) -> Farm {
    Farm::new(
        Arc::new(kbf_front::Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
    .with_cpu_admission(admission)
}

fn admission() -> Option<CpuAdmission> {
    Some(CpuAdmission::new(50, Duration::from_secs(5)))
}

fn worker(name: &str) -> WorkerId {
    WorkerId::new(name)
}

/// `name` registered on a new stream, which stays open for the rest of the test.
fn register(farm: &Farm, name: &str) -> StreamId {
    let caps = kbf_caps::NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    let (outbound, inbox) = tokio::sync::mpsc::unbounded_channel();
    // The receiver lives as long as the test: a dropped one closes the stream.
    std::mem::forget(inbox);
    farm.register(
        &worker(name),
        DaemonInstance::new(name),
        DaemonInstance::new(""),
        Reported::serving(Resources::new(4_000, 64 << 30), caps),
        outbound,
        ServerMessage::default(),
    )
}

/// Queues an action distinguished by `tag`.
fn submit(farm: &Farm, tag: u8) {
    let request = Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
        },
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
    };
    let ticket = farm.submit(Submission {
        request,
        call: None,
        described: Described::default(),
        evidence: Default::default(),
    });
    drop(ticket.expect("queued"));
}

/// Asserts that a reading a heartbeat carries reaches placement: a node under a full
/// reading takes no second action (the first, on an empty node, is let in), and a reading of
/// idle admits it at once, without waiting for a tick. Catches a reading that is dropped on
/// the way from the heartbeat (the farm admitting as if the daemon sent none), and one that
/// is fed but does not place.
#[tokio::test]
async fn a_reading_reaches_placement() {
    let farm = farm(admission());
    let stream = register(&farm, "a");
    submit(&farm, 1);
    farm.tick().await;
    assert_eq!(farm.queued_operations(), 0, "an empty node takes one");

    farm.note_busy(&worker("a"), stream, Some(4_000));
    submit(&farm, 2);
    farm.tick().await;
    assert_eq!(farm.queued_operations(), 1, "a full node takes no more");

    farm.note_busy(&worker("a"), stream, Some(0));
    assert_eq!(farm.queued_operations(), 0, "an idle reading places it");
}

/// Asserts that a heartbeat with no reading changes nothing (the reading the scheduler holds
/// stays until it ages out), and that one from a stream the node's newer stream replaced, or
/// from a node that never registered, is ignored. Catches a missing reading fed as 0 (idle),
/// which would open a busy node, and a stale stream's reading steering the current one.
#[tokio::test]
async fn a_heartbeat_without_a_reading_or_from_a_stale_stream_changes_nothing() {
    let farm = farm(admission());
    let stream = register(&farm, "a");
    submit(&farm, 1);
    farm.tick().await;
    farm.note_busy(&worker("a"), stream, Some(4_000));
    submit(&farm, 2);
    farm.tick().await;
    assert_eq!(farm.queued_operations(), 1);

    farm.note_busy(&worker("a"), stream, None);
    farm.tick().await;
    assert_eq!(farm.queued_operations(), 1, "no reading is not an idle one");

    farm.note_busy(&worker("a"), StreamId(u64::MAX), Some(0));
    farm.note_busy(&worker("ghost"), stream, Some(0));
    assert_eq!(farm.queued_operations(), 1);
}

/// Asserts that a farm without the feature (the default) places by bookings alone whatever
/// the readings say.
#[tokio::test]
async fn without_the_feature_readings_hold_nothing_back() {
    let farm = farm(None);
    let stream = register(&farm, "a");
    submit(&farm, 1);
    farm.tick().await;
    farm.note_busy(&worker("a"), stream, Some(4_000));
    submit(&farm, 2);
    farm.tick().await;
    assert_eq!(farm.queued_operations(), 0);
}
