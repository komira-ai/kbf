//! The pack-write counts the farm keeps for each node, from the heartbeats of its streams.

use std::sync::Arc;

use kbf_proto::worker::{PackWriteCounts, ServerMessage};
use kbf_sched::DaemonInstance;
use kbf_types::{Resources, WorkerId};

use crate::farm::phase::Reported;
use crate::farm::{Farm, Outbound, StreamId};

fn farm() -> Farm {
    Farm::new(
        Arc::new(kbf_front::Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
}

fn worker(name: &str) -> WorkerId {
    WorkerId::new(name)
}

/// Counts whose `packs` is `packs` and the rest 0.
fn counts(packs: u64) -> PackWriteCounts {
    PackWriteCounts {
        packs,
        ..PackWriteCounts::default()
    }
}

/// What a node's `Hello` reports: serving, with room to run a lease.
fn serving() -> Reported {
    let report = [("arch", "x86_64"), ("os", "linux")];
    let caps = kbf_caps::NodeCaps::from_report(report).expect("caps");
    Reported::serving(
        Resources::new(4_000, 64 << 30),
        caps.with_drivers(["container"]),
    )
}

/// `name` registered on a new stream that sends to `outbound`.
fn register_over(farm: &Farm, name: &str, outbound: Outbound) -> StreamId {
    let instance = DaemonInstance::new(name);
    let predecessor = DaemonInstance::new("");
    let welcome = ServerMessage::default();
    farm.register(
        &worker(name),
        instance,
        predecessor,
        serving(),
        outbound,
        welcome,
    )
}

/// `name` registered on a new stream, which stays open for the rest of the test.
fn register(farm: &Farm, name: &str) -> StreamId {
    let (outbound, inbox) = tokio::sync::mpsc::unbounded_channel();
    // The receiver lives as long as the test: a dropped one closes the stream.
    std::mem::forget(inbox);
    register_over(farm, name, outbound)
}

/// Asserts that the counts on a heartbeat are kept as the node's newest, each node has its own,
/// they are listed in node-id order, and a node that has sent none is not listed.
#[test]
fn the_newest_counts_of_each_node_are_kept() {
    let farm = farm();
    let b = register(&farm, "b");
    let a = register(&farm, "a");
    register(&farm, "c");
    assert!(farm.pack_writes().is_empty());

    farm.note_pack_writes(&worker("b"), b, Some(counts(2)));
    farm.note_pack_writes(&worker("a"), a, Some(counts(1)));
    let want = [(worker("a"), counts(1)), (worker("b"), counts(2))];
    assert_eq!(farm.pack_writes(), want);

    farm.note_pack_writes(&worker("a"), a, Some(counts(5)));
    let want = [(worker("a"), counts(5)), (worker("b"), counts(2))];
    assert_eq!(farm.pack_writes(), want);
}

/// Asserts that a heartbeat without counts forgets the node's, so a daemon restarted without
/// `--pack-write` does not show the counts of the process before it, and leaves the other
/// nodes' alone.
#[test]
fn a_heartbeat_without_counts_forgets_the_nodes() {
    let farm = farm();
    let (a, b) = (register(&farm, "a"), register(&farm, "b"));
    farm.note_pack_writes(&worker("a"), a, Some(counts(1)));
    farm.note_pack_writes(&worker("b"), b, Some(counts(2)));
    farm.note_pack_writes(&worker("a"), a, None);
    assert_eq!(farm.pack_writes(), [(worker("b"), counts(2))]);
}

/// Asserts that counts on a stream the node's newer stream replaced are ignored, and so is a
/// heartbeat without counts on it, and those on a node that never registered.
#[test]
fn counts_on_a_replaced_stream_or_an_unknown_node_are_ignored() {
    let farm = farm();
    let first = register(&farm, "a");
    let second = register(&farm, "a");
    farm.note_pack_writes(&worker("a"), first, Some(counts(1)));
    assert!(farm.pack_writes().is_empty());

    farm.note_pack_writes(&worker("a"), second, Some(counts(2)));
    farm.note_pack_writes(&worker("a"), first, None);
    farm.note_pack_writes(&worker("ghost"), StreamId(0), Some(counts(9)));
    assert_eq!(farm.pack_writes(), [(worker("a"), counts(2))]);
}

/// Asserts that a node whose stream has closed keeps the counts of its last heartbeat.
#[test]
fn a_node_that_is_gone_keeps_its_last_counts() {
    let farm = farm();
    let (outbound, inbox) = tokio::sync::mpsc::unbounded_channel();
    let stream = register_over(&farm, "a", outbound);
    farm.note_pack_writes(&worker("a"), stream, Some(counts(3)));

    drop(inbox);
    let looks = farm.update_look();
    assert_eq!(looks.len(), 1);
    assert!(!looks[0].connected, "the node's stream is closed");
    assert_eq!(farm.pack_writes(), [(worker("a"), counts(3))]);
}
