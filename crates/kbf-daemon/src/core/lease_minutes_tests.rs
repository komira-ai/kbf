//! The lease core's use of the lease minutes collector: a core over a scripted runtime, no
//! stream, readings the test chose. The paused clock stands in for a reading that never
//! comes.

use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::NodeLoad;

use super::*;
use crate::clock::SystemClock;
use crate::phases::LeasePhases;
use crate::runtime::{FakeRuntime, LeaseReading, LeaseUsage, NodeLoadError, NodeReading};
use crate::runtimes::Runtimes;

const S: u64 = 1_000_000;

fn core_over(runtime: FakeRuntime) -> LeaseCore {
    let (done, _) = mpsc::unbounded_channel();
    let mut core = LeaseCore::new(
        Leases::new(Runtimes::new(Arc::new(runtime)), done),
        Duration::from_secs(40),
        8,
        Arc::new(SystemClock),
    );
    let now = core.clock.now();
    core.new_stream(Duration::from_secs(10), Duration::from_secs(10), now);
    core
}

/// A runtime that answers `readings` in order.
fn answering(
    readings: impl IntoIterator<Item = Result<Option<NodeReading>, NodeLoadError>>,
) -> FakeRuntime {
    readings.into_iter().fold(
        FakeRuntime::new(Duration::ZERO),
        FakeRuntime::with_node_load,
    )
}

/// The reading at `at_s` of lease (1, 1), started at 0, at half a CPU.
fn at(at_s: u64) -> Result<Option<NodeReading>, NodeLoadError> {
    Ok(Some(NodeReading {
        load: NodeLoad {
            seq: at_s,
            mono_usec: at_s * S,
            cpu_usage_usec: 7,
            memory_working_set_bytes: 11,
            actions_cpu_limited: true,
            ..NodeLoad::default()
        },
        leases: vec![LeaseReading {
            lease: LeaseId::new(1, 1),
            started_usec: 0,
            usage: Some(LeaseUsage {
                cpu_usage_usec: at_s * S / 2,
                working_set_bytes: at_s,
            }),
        }],
    }))
}

fn broke(kind: &'static str) -> Result<Option<NodeReading>, NodeLoadError> {
    Err(NodeLoadError {
        kind,
        why: "unreadable".to_owned(),
    })
}

/// The beats of a core that read at 0, 10, ..., `last_s`, numbered from 1.
async fn beats_to(core: &mut LeaseCore, last_s: u64) -> Vec<NodeLoad> {
    let mut out = Vec::new();
    for (n, _) in (0..=last_s).step_by(10).enumerate() {
        out.push(core.node_load(n as u64 + 1).await.expect("a load"));
    }
    out
}

/// Asserts that the load a beat returns is the reading's with the start clock of the lease,
/// the covered list and the closed minutes put in, and that the node's own counters are the
/// reading's, untouched.
#[tokio::test]
async fn node_load_attaches_clocks_and_minutes() {
    let mut core = core_over(answering((0..=60).step_by(10).map(at)));
    let loads = beats_to(&mut core, 60).await;
    let last = &loads[6];
    assert_eq!(
        (
            last.mono_usec,
            last.cpu_usage_usec,
            last.memory_working_set_bytes
        ),
        (60 * S, 7, 11)
    );
    assert!(last.actions_cpu_limited);
    assert_eq!(last.lease_clocks.len(), 1);
    assert_eq!(last.lease_clocks[0].started_mono_usec, 0);
    assert_eq!(last.minutes.len(), 1);
    assert_eq!(last.minutes[0].cpu_mean_millis, 500);
    assert!(loads[..6].iter().all(|load| load.minutes.is_empty()));
}

/// Asserts that a reading that fails, and one the runtime does not take, send no load and
/// take nothing from the collector, and that a failure is logged once per kind.
#[tokio::test]
async fn a_failed_reading_takes_nothing() {
    let mut core = core_over(answering([broke("cpu.stat"), broke("cpu.stat"), Ok(None)]));
    for seq in 1..=3 {
        assert_eq!(core.node_load(seq).await, None);
    }
    assert_eq!(core.minutes.in_flight(), None);
    assert_eq!(core.minutes.pending(), 0);
    assert_eq!(core.load_errors.len(), 1);
    assert!(core.load_errors.contains("cpu.stat"));
}

/// Asserts that a heartbeat's acknowledgement through the core frees the batch that
/// heartbeat carried.
#[tokio::test]
async fn an_ack_through_the_core_frees_the_batch() {
    let mut core = core_over(answering((0..=60).step_by(10).map(at)));
    beats_to(&mut core, 60).await;
    assert_eq!(core.minutes.in_flight(), Some(7));
    core.on_heartbeat_ack(7);
    assert_eq!(core.minutes.in_flight(), None);
}

/// Asserts that a lease that finishes through the core closes its open minute, which waits
/// for the next heartbeat.
#[tokio::test]
async fn a_finished_lease_closes_its_minute() {
    let mut core = core_over(answering([at(0), at(10), at(20), at(30), at(40), at(45)]));
    for seq in 1..=6 {
        assert!(
            core.node_load(seq)
                .await
                .expect("a load")
                .minutes
                .is_empty()
        );
    }
    let id = LeaseId::new(1, 1);
    let phases = LeasePhases::new(id, "scripted", Duration::ZERO, None);
    core.finished(None, (id, Ok(ActionResult::default()), None, phases));
    assert_eq!(core.minutes.pending(), 1);
}

/// Asserts that a new stream through the core reaches the collector: the batch a stream left
/// unacknowledged is waiting to be sent again.
#[tokio::test]
async fn new_stream_reaches_the_collector() {
    let mut core = core_over(answering((0..=60).step_by(10).map(at)));
    beats_to(&mut core, 60).await;
    assert_eq!(core.minutes.in_flight(), Some(7));
    let now = core.clock.now();
    core.new_stream(Duration::from_secs(10), Duration::from_secs(10), now);
    assert_eq!(core.minutes.in_flight(), None);
    assert_eq!(core.minutes.pending(), 1);
}

/// Asserts that a reading that outlives its deadline gives a beat without a load, logged
/// once, and takes nothing from the collector. The clock is paused, so nothing here depends
/// on how long anything takes.
#[tokio::test(start_paused = true)]
async fn a_reading_that_outlives_its_deadline_gives_a_beat_without_load() {
    let mut core = core_over(FakeRuntime::new(Duration::ZERO).with_hanging_node_load());
    assert_eq!(core.node_load(1).await, None);
    assert_eq!(core.node_load(2).await, None);
    assert_eq!(core.load_errors.len(), 1);
    assert!(core.load_errors.contains("deadline"));
    assert_eq!(core.minutes.in_flight(), None);
    assert_eq!(core.minutes.pending(), 0);
}
