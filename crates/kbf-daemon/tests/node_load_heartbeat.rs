//! The node load a daemon sends on its heartbeats (`Heartbeat.node_load`) and the busiest CPU
//! window of a lease in its usage record (`CgroupUsage.peak_window_millicores`), from a runtime
//! that takes readings. The runtime here is scripted: it answers the readings it is given.

mod support;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_daemon::{NodeLoadError, NodeReading, Runtime, RuntimeError, Work};
use kbf_proto::reapi::{ActionResult, ExecutedActionMetadata};
use kbf_proto::worker::{CgroupUsage, NodeLoad, ResourceUsage};
use kbf_types::LeaseId;
use support::{Harness, PROMPT};

/// A runtime that answers `readings` in order (then `Ok(None)`), ends each lease with `outcome`
/// once, reports `failed` as what it measured of an erroring run, and says `peak` for the first
/// lease that asks.
struct Scripted {
    readings: Mutex<VecDeque<Result<Option<NodeReading>, NodeLoadError>>>,
    outcome: Mutex<Option<Result<ActionResult, RuntimeError>>>,
    failed: Mutex<Option<ResourceUsage>>,
    peak: Mutex<Option<u64>>,
}

impl Scripted {
    fn new() -> Self {
        Self {
            readings: Mutex::new(VecDeque::new()),
            outcome: Mutex::new(None),
            failed: Mutex::new(None),
            peak: Mutex::new(None),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Runtime for Scripted {
    fn driver(&self) -> &'static str {
        "scripted"
    }

    fn serves(&self, kind: &str) -> bool {
        kind == "action"
    }

    async fn run(&self, _work: Work) -> Result<ActionResult, RuntimeError> {
        lock(&self.outcome)
            .take()
            .unwrap_or_else(|| Ok(ActionResult::default()))
    }

    fn failed_usage(&self, _lease_id: LeaseId) -> Option<ResourceUsage> {
        lock(&self.failed).take()
    }

    async fn node_load(&self) -> Result<Option<NodeReading>, NodeLoadError> {
        lock(&self.readings).pop_front().unwrap_or(Ok(None))
    }

    fn take_peak_window_millicores(&self, _lease_id: LeaseId) -> Option<u64> {
        lock(&self.peak).take()
    }

    async fn kill(&self, _lease_id: LeaseId) {}
}

fn reading(seq: u64) -> NodeLoad {
    NodeLoad {
        seq,
        mono_usec: seq * 1_000,
        cpu_usage_usec: seq * 10,
        memory_working_set_bytes: 4096,
        memory_active_file_bytes: 512,
        covered: vec![kbf_proto::worker::LeaseId { term: 1, seq: 2 }],
        actions_cpu_limited: true,
        cpu_throttled_usec: Some(5),
        cpu_pressure_some_total_usec: None,
        lease_clocks: Vec::new(),
        minutes: Vec::new(),
    }
}

fn broke(kind: &'static str) -> Result<Option<NodeReading>, NodeLoadError> {
    Err(NodeLoadError {
        kind,
        why: "unreadable".to_owned(),
    })
}

fn usage_result(usage: &ResourceUsage) -> ActionResult {
    ActionResult {
        execution_metadata: Some(ExecutedActionMetadata {
            auxiliary_metadata: vec![usage.to_any()],
            ..ExecutedActionMetadata::default()
        }),
        ..ActionResult::default()
    }
}

fn measured() -> ResourceUsage {
    ResourceUsage {
        cpu_user_micros: 9,
        cgroup: Some(CgroupUsage {
            cpu_usage_micros: Some(1_000),
            ..CgroupUsage::default()
        }),
        ..ResourceUsage::default()
    }
}

/// Runs one lease on `runtime` and returns the usage its Result carries, if any.
async fn usage_of_a_lease(name: &str, runtime: Arc<Scripted>) -> Option<ResourceUsage> {
    let mut h = Harness::with_runtime(name, runtime, Duration::from_secs(60)).await;
    let mut peer = h.welcomed().await;
    peer.start(1, 1, "action");
    let (_, result) = peer.result(PROMPT).await.expect("a Result");
    result
        .action_result
        .as_ref()
        .and_then(ResourceUsage::in_result)
}

/// Asserts that each heartbeat carries the reading the runtime gives when it gives one, leaves
/// `node_load` out when the runtime gives none or fails (the same kind of failure twice, and
/// another kind, each leaving it out), and carries the next reading that succeeds, field for
/// field.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reading_reaches_the_heartbeat_and_a_failed_one_leaves_it_unset() {
    let runtime = Arc::new(Scripted::new());
    lock(&runtime.readings).extend([
        Ok(Some(reading(1).into())),
        broke("cpu.stat"),
        broke("cpu.stat"),
        broke("memory.stat"),
        Ok(None),
        Ok(Some(reading(2).into())),
    ]);
    let mut h = Harness::with_runtime(
        "node-load-heartbeat",
        Arc::clone(&runtime),
        Duration::from_secs(60),
    )
    .await;
    let mut peer = h.welcomed().await;
    let mut seen = Vec::new();
    for _ in 0..6 {
        seen.push(peer.heartbeat().await.node_load);
    }
    let want = [Some(reading(1)), None, None, None, None, Some(reading(2))];
    // Heartbeats are numbered in the order they were sent, one reading each.
    assert_eq!(seen, want);
}

/// Asserts that a runtime that takes no readings (the default) sends no `node_load`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runtime_without_readings_sends_none() {
    let runtime = Arc::new(kbf_daemon::FakeRuntime::new(Duration::ZERO));
    let mut h = Harness::with_runtime("node-load-none", runtime, Duration::from_secs(60)).await;
    let mut peer = h.welcomed().await;
    assert_eq!(peer.heartbeat().await.node_load, None);
}

/// Asserts that the peak window the runtime says after a lease ended is in the usage its
/// successful Result carries, beside the rest of the usage, and that a lease with no peak
/// carries its usage as the runtime gave it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_successful_lease_carries_its_peak_window() {
    let runtime = Arc::new(Scripted::new());
    *lock(&runtime.outcome) = Some(Ok(usage_result(&measured())));
    *lock(&runtime.peak) = Some(2_500);
    let usage = usage_of_a_lease("peak-ok", runtime).await.expect("usage");
    let cgroup = usage.cgroup.expect("cgroup");
    assert_eq!(cgroup.peak_window_millicores, Some(2_500));
    assert_eq!(cgroup.cpu_usage_micros, Some(1_000));
    assert_eq!(usage.cpu_user_micros, 9);

    let runtime = Arc::new(Scripted::new());
    *lock(&runtime.outcome) = Some(Ok(usage_result(&measured())));
    let usage = usage_of_a_lease("peak-none", runtime).await;
    assert_eq!(usage, Some(measured()));
}

/// Asserts that a lease that ended in an error carries the peak in the usage its runtime
/// measured, and that a peak with no usage to put it in, or in a result that carries none, is
/// dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_lease_carries_its_peak_window_in_the_usage_the_runtime_measured() {
    let runtime = Arc::new(Scripted::new());
    *lock(&runtime.outcome) = Some(Err(RuntimeError::Killed));
    *lock(&runtime.failed) = Some(measured());
    *lock(&runtime.peak) = Some(750);
    let usage = usage_of_a_lease("peak-failed", runtime)
        .await
        .expect("usage");
    assert_eq!(
        usage.cgroup.expect("cgroup").peak_window_millicores,
        Some(750)
    );

    let runtime = Arc::new(Scripted::new());
    *lock(&runtime.outcome) = Some(Err(RuntimeError::Killed));
    *lock(&runtime.peak) = Some(750);
    assert_eq!(usage_of_a_lease("peak-no-usage", runtime).await, None);

    let runtime = Arc::new(Scripted::new());
    *lock(&runtime.peak) = Some(750);
    assert_eq!(usage_of_a_lease("peak-no-metadata", runtime).await, None);
}
