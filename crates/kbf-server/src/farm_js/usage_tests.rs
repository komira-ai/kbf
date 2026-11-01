//! Tests of the farm's usage side ([`super::usage`]) with no farm around it: a panic in the hub,
//! the tick's schedule, the bound on runs, and what an action is noted as. The farm's own
//! behaviour with it (beats, results, the farm's handover) is in `farm/js/tests/usage.rs`.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use js_types::HostId;
use js_usage::{
    KeyProfile, Limits, LoadIn, NodeMinute, Observation, ProfileCurves, ProfileKey, RunEnd,
    SinkCounters, UsageCounters, UsageFault, UsageSink,
};
use kbf_front::Described;
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use kbf_types::sched::usage::{LeaseMinute, NodeLoad};
use kbf_types::{ActionKey, Digest, DigestFunction, LeaseId, OperationId, WaiterId, WorkerId};
use kbf_usage::{KbfUsageSink, MAX_SHAPES, profile_name};

use super::usage::{Usage, UsageStats};
use crate::action_usage::Subject;
use crate::machine::Platform;

fn store() -> Arc<dyn Store> {
    let store = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    kbf_state::share(&*store).expect("the shared marker");
    store
}

fn action(tag: u8) -> ActionKey {
    ActionKey {
        instance: "main".to_owned(),
        action: Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
    }
}

/// The sink the hub calls, which panics where it is told to.
struct Panicky {
    inner: Arc<KbfUsageSink>,
    on_observe: AtomicBool,
    on_flush: AtomicBool,
}

impl UsageSink for Panicky {
    fn observe(
        &self,
        host: &HostId,
        epoch: u64,
        load: &LoadIn,
        limits: Limits,
        now_minute: u64,
    ) -> Observation {
        assert!(!self.on_observe.load(Ordering::SeqCst), "told to panic");
        self.inner.observe(host, epoch, load, limits, now_minute)
    }

    fn run_ended(&self, end: &RunEnd, now_minute: u64) -> Result<(), UsageFault> {
        self.inner.run_ended(end, now_minute)
    }

    fn flush(&self, now_minute: u64) -> Result<Vec<(HostId, NodeMinute)>, UsageFault> {
        assert!(!self.on_flush.load(Ordering::SeqCst), "told to panic");
        self.inner.flush(now_minute)
    }

    fn prune(&self, now_minute: u64) -> Result<(), UsageFault> {
        self.inner.prune(now_minute)
    }

    fn node_rows(
        &self,
        host: &HostId,
        from: u64,
        to: u64,
        limit: NonZeroUsize,
    ) -> Result<Vec<NodeMinute>, UsageFault> {
        self.inner.node_rows(host, from, to, limit)
    }

    fn key_profile(&self, key: &ProfileKey) -> Result<Option<KeyProfile>, UsageFault> {
        self.inner.key_profile(key)
    }

    fn curves(&self) -> Option<Arc<ProfileCurves>> {
        self.inner.curves()
    }

    fn counters(&self) -> SinkCounters {
        self.inner.counters()
    }
}

/// A usage side whose sink panics where `on_observe` and `on_flush` say.
fn panicky() -> (Usage, Arc<Panicky>) {
    let made = std::sync::Mutex::new(None);
    let usage = Usage::open_through(store(), 0, |seam| {
        let sink = Arc::new(Panicky {
            inner: seam,
            on_observe: AtomicBool::new(false),
            on_flush: AtomicBool::new(false),
        });
        *made.lock().expect("lock") = Some(Arc::clone(&sink));
        sink
    })
    .expect("a usage side");
    let sink = made.into_inner().expect("lock").expect("a sink");
    (usage, sink)
}

fn worker() -> WorkerId {
    WorkerId::new("node-u")
}

fn load() -> NodeLoad {
    NodeLoad {
        seq: 1,
        mono_usec: 1_000_000,
        minutes: vec![LeaseMinute {
            lease: LeaseId::new(1, 1),
            minute: 0,
            cpu_mean_millis: 1_000,
            cpu_peak_window_millis: 1_001,
            ws_max_bytes: 5_000,
        }],
        ..NodeLoad::default()
    }
}

/// Catches a panic in the hub reaching the heartbeat that called it, or leaving the run it
/// carried bound: the beat returns, one fault is counted, and the run is forgotten.
#[test]
fn a_panic_in_the_hub_is_counted_and_the_run_is_forgotten() {
    let (usage, sink) = panicky();
    usage.accepted(&worker(), LeaseId::new(1, 1), action(1), true, 0);
    assert_eq!(usage.stats().held_runs, 1);
    sink.on_observe.store(true, Ordering::SeqCst);
    usage.beat_blocking(&worker(), 1, Some(&load()), &BTreeSet::new(), 0);
    let stats = usage.stats();
    assert_eq!(stats.counters.faults, 1);
    assert_eq!(stats.held_runs, 0);
    assert_eq!(stats.waiting, 0);
}

/// Catches a panic in a tick's flush or the final flush escaping: each is counted as a fault,
/// and the final flush says whether the store took it.
#[test]
fn a_panic_in_a_flush_is_counted() {
    let (usage, sink) = panicky();
    sink.on_flush.store(true, Ordering::SeqCst);
    usage.tick_blocking(10);
    assert_eq!(usage.stats().counters.faults, 1);
    assert!(!usage.finish_blocking(10));
    assert_eq!(usage.stats().counters.faults, 2);
    sink.on_flush.store(false, Ordering::SeqCst);
    assert!(usage.finish_blocking(10));
    assert_eq!(usage.stats().counters.faults, 2);
}

/// Catches a tick that runs on every call, or never: it runs for the first call and then
/// once for each five wall minutes, and each run is on the blocking pool.
#[tokio::test]
async fn the_tick_runs_once_for_each_five_wall_minutes() {
    let usage = Arc::new(Usage::open(store(), 100).expect("a usage side"));
    usage.tick(100).await;
    assert_eq!(usage.stats().ticks, 1);
    usage.tick(101).await;
    usage.tick(104).await;
    assert_eq!(usage.stats().ticks, 1);
    usage.tick(105).await;
    assert_eq!(usage.stats().ticks, 2);
    assert!(usage.finish(105).await);
}

/// Catches a table of runs without a bound: with it full a run is refused, counted, and not
/// held.
#[test]
fn a_full_table_of_runs_refuses_the_bind_and_counts_it() {
    let usage = Usage::open(store(), 0).expect("a usage side");
    for n in 0..MAX_SHAPES as u64 {
        usage.accepted(&worker(), LeaseId::new(1, n), action(1), true, 0);
    }
    assert_eq!(usage.stats().binds_refused, 0);
    usage.accepted(&worker(), LeaseId::new(2, 0), action(1), true, 0);
    let stats = usage.stats();
    assert_eq!(stats.binds_refused, 1);
    assert_eq!(stats.held_runs, MAX_SHAPES);
    assert_eq!(stats.waiting, MAX_SHAPES);
}

/// Catches an action named when the estimator has no key for it, or not named when it has: a
/// request that does not pin the OS is left out, a Linux one is kept under its profile name.
#[test]
fn an_action_is_noted_only_when_the_estimator_can_key_it() {
    let usage = Usage::open(store(), 0).expect("a usage side");
    let described = Described {
        arguments: vec!["compile".to_owned()],
        ..Described::default()
    };
    let subject = Subject::of(described).0;
    let linux = Platform::of(
        &kbf_caps::Request::from_platform([("OSFamily", "linux")]).expect("a platform"),
    );
    let unpinned = Platform::of(&kbf_caps::Request::default());
    usage.note_action(&action(1), &unpinned, &subject);
    assert_eq!(usage.shapes().len(), 0);
    usage.note_action(&action(1), &linux, &subject);
    usage.note_action(&action(2), &linux, &subject);
    assert_eq!(usage.shapes().len(), 2);
    let key = crate::farm_js::learning::estimator_key("main", &linux, &subject).expect("a key");
    assert_eq!(usage.shapes().get(&action(2)), Some(profile_name(&key)));
}

/// Catches a family renamed, dropped, or given the wrong type: the text of a scrape for known
/// counts.
#[test]
fn the_usage_families_render_the_counts() {
    let stats = UsageStats {
        counters: UsageCounters {
            runs_learned: 3,
            runs_without_key: 2,
            runs_without_series: 1,
            loads_rejected: 4,
            minutes_unheld: 5,
            faults: 6,
            ..UsageCounters::default()
        },
        declined: 7,
        binds_refused: 8,
        keys: 9,
        bytes: 10,
        held_runs: 11,
        waiting: 12,
        ticks: 13,
        shapes_evicted: 16,
        results_held: 14,
        results_dropped: 15,
    };
    let mut out = String::new();
    stats.render(&mut out);
    for (name, kind, value) in [
        ("kbf_usage_runs_learned_total", "counter", 3),
        ("kbf_usage_runs_declined_total", "counter", 7),
        ("kbf_usage_runs_without_key_total", "counter", 2),
        ("kbf_usage_runs_without_series_total", "counter", 1),
        ("kbf_usage_binds_refused_total", "counter", 8),
        ("kbf_usage_loads_rejected_total", "counter", 4),
        ("kbf_usage_minutes_unheld_total", "counter", 5),
        ("kbf_usage_faults_total", "counter", 6),
        ("kbf_usage_ticks_total", "counter", 13),
        ("kbf_usage_shapes_evicted_total", "counter", 16),
        ("kbf_usage_profile_keys", "gauge", 9),
        ("kbf_usage_profile_bytes", "gauge", 10),
        ("kbf_usage_runs_held", "gauge", 11),
    ] {
        let expected = format!("# TYPE {name} {kind}\n{name} {value}\n");
        assert!(out.contains(&expected), "{expected} not in {out}");
    }
    for (reason, value) in [("held", 14), ("dropped", 15)] {
        let expected =
            format!("kbf_usage_results_not_taken_total{{reason=\"{reason}\"}} {value}\n");
        assert!(out.contains(&expected), "{expected} not in {out}");
    }
    assert!(out.contains("# TYPE kbf_usage_results_not_taken_total counter\n"));
    assert_eq!(out.matches("# HELP").count(), 14);
}

/// Pins what the adapter does with a row that kept no subject (it arrives green): an
/// operation read back with no parcel is not named, because the empty subject has no
/// estimator key, so it cannot overwrite the entry of a real action; one with a parcel is.
#[test]
fn a_row_with_no_parcel_is_not_noted() {
    use js_server_core::Adapter as _;
    let usage = Arc::new(Usage::open(store(), 0).expect("a usage side"));
    let mut adapter = super::testing::adapter().noting_usage(Some(Arc::clone(&usage)));
    let request = kbf_sched::Request {
        needs: kbf_caps::Request::from_platform([("OSFamily", "linux")]).expect("a platform"),
        ..super::testing::request(1)
    };
    adapter.adopted(OperationId(1), WaiterId(1), None, &request, None, None);
    assert_eq!(usage.shapes().len(), 0);
    let described = Described {
        arguments: vec!["compile".to_owned()],
        ..Described::default()
    };
    let parcel = crate::farm::durable::subject::encode(&Subject::of(described).0);
    adapter.adopted(
        OperationId(2),
        WaiterId(2),
        Some(&parcel),
        &request,
        None,
        None,
    );
    assert_eq!(usage.shapes().len(), 1);
}
