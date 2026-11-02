//! Tests of what the engine records beside the latencies of a pass: the busy time of the disks
//! over it, the requests the fault proxy forwarded around it by method, the most requests that
//! were in flight in it, and the request-rate caps of the run. Compiled for tests only.
//!
//! The pass lasts as long as its request sleeps on tokio's paused clock, so the wall time of a
//! pass is a number the test chose and no test reads the real clock.

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_testkit::dir::TestDir;
use tokio::sync::Barrier;

use crate::cells::CellId;
use crate::cells::engine::{Arm, Env, FaultSwitch, LoadSource, Pace, Step, run_cell, work};
use crate::cells::testkit::{self, Script, calm};
use crate::config::RateCaps;
use crate::error::BenchError;
use crate::gate::LoadSample;
use crate::proxy::FaultMode;
use crate::result::{CellResult, Op, Window};
use crate::role::Role;

fn temp(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), name)
}

fn step() -> Step {
    Step {
        label: "",
        second_quantity: false,
        role: Role::Cold,
        op: Op::Put,
        bytes: 10,
        started_unix_ms: 5,
        latency_us: 100,
        status: Some(200),
        ok: true,
        cold_write: 0,
        in_flight: 0,
    }
}

/// An arm of `requests` requests a pass at `in_flight` workers, each of which takes `secs`
/// seconds of the paused clock.
fn arm(
    label: &str,
    requests: u64,
    in_flight: u32,
    secs: u64,
    proxy: Option<Arc<dyn FaultSwitch>>,
) -> Arm {
    Arm {
        label: label.to_owned(),
        pace: Pace::Closed,
        in_flight,
        requests,
        cold_bytes_per_pass: 0,
        cold_bytes_once: 0,
        work: work(move |_| async move {
            tokio::time::sleep(Duration::from_secs(secs)).await;
            Ok(vec![step()])
        }),
        before: None,
        after: None,
        faulted: false,
        proxy,
        notes: vec![],
    }
}

/// A machine that is calm and reports the busy time of its disks as scripted, one reading per
/// call, the last for ever.
struct Disks(Mutex<VecDeque<BTreeMap<String, u64>>>);

impl Disks {
    fn of(readings: &[&[(&str, u64)]]) -> Arc<Self> {
        Arc::new(Self(Mutex::new(
            readings
                .iter()
                .map(|r| r.iter().map(|(d, ms)| ((*d).to_owned(), *ms)).collect())
                .collect(),
        )))
    }
}

impl LoadSource for Disks {
    fn sample(&self) -> Result<LoadSample, BenchError> {
        Ok(calm())
    }

    fn disk_busy_ms(&self) -> Result<BTreeMap<String, u64>, BenchError> {
        let mut readings = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if readings.len() > 1 {
            Ok(readings.pop_front().unwrap_or_default())
        } else {
            Ok(readings.front().cloned().unwrap_or_default())
        }
    }
}

/// A fault proxy that reports the counts by method it was scripted with, one reading per call,
/// the last for ever.
struct CountedProxy(Mutex<VecDeque<BTreeMap<String, u64>>>);

impl CountedProxy {
    fn of(readings: &[&[(&str, u64)]]) -> Arc<Self> {
        Arc::new(Self(Mutex::new(
            readings
                .iter()
                .map(|r| r.iter().map(|(m, n)| ((*m).to_owned(), *n)).collect())
                .collect(),
        )))
    }
}

impl FaultSwitch for CountedProxy {
    fn set_mode(&self, _: FaultMode) -> Result<(), BenchError> {
        Ok(())
    }

    fn request_count(&self) -> u64 {
        0
    }

    fn method_counts(&self) -> BTreeMap<String, u64> {
        let mut readings = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if readings.len() > 1 {
            readings.pop_front().unwrap_or_default()
        } else {
            readings.front().cloned().unwrap_or_default()
        }
    }
}

fn counts(pairs: &[(&str, u64)]) -> BTreeMap<String, u64> {
    pairs.iter().map(|(m, n)| ((*m).to_owned(), *n)).collect()
}

fn env_with(dir: &Path, load: Arc<dyn LoadSource>) -> Env {
    Env::new(&testkit::ctx(dir, &[], Window::Quiet), load).unwrap()
}

/// Catches a pass that records nothing of the disks, a disk's busy time taken from the wrong
/// reading (the whole of it instead of the pass's share), a utilisation that is not the busy
/// time over the wall time of the pass, and a disk that is idle being left out: two passes of
/// two seconds, one disk busy 400 ms in the first and 600 ms in the second, another idle.
#[tokio::test(start_paused = true)]
async fn a_pass_records_how_long_each_disk_was_busy_over_it() {
    let dir = temp("tb-engine-disks");
    let disks = Disks::of(&[
        &[("sda", 1_000), ("sdb", 70)],
        &[("sda", 1_400), ("sdb", 70)],
        &[("sda", 1_400), ("sdb", 70)],
        &[("sda", 2_000), ("sdb", 70)],
    ]);
    let env = env_with(dir.path(), disks);
    let arms = vec![arm("a", 1, 1, 2, None)];
    let results = run_cell(&env, CellId::ColdPut, arms, 2, None)
        .await
        .unwrap();
    let passes = &results[0].passes;
    for (pass, busy_ms, util) in [(0, 400, 20.0), (1, 600, 30.0)] {
        let sda = passes[pass].disk_busy["sda"];
        assert_eq!(passes[pass].elapsed_us, 2_000_000);
        assert_eq!(sda.busy_ms, busy_ms, "pass {pass}");
        assert!((sda.util_pct - util).abs() < 1e-9, "pass {pass}: {sda:?}");
        let sdb = passes[pass].disk_busy["sdb"];
        assert_eq!((sdb.busy_ms, sdb.util_pct), (0, 0.0));
    }
}

/// Catches a machine that cannot say anything of its disks (the default of a load source)
/// being written as disks that were idle.
#[tokio::test(start_paused = true)]
async fn a_machine_that_reports_no_disks_leaves_the_field_empty() {
    let dir = temp("tb-engine-nodisks");
    let env = env_with(dir.path(), Script::calm());
    let arms = vec![arm("a", 1, 1, 1, None)];
    let results = run_cell(&env, CellId::ColdPut, arms, 2, None)
        .await
        .unwrap();
    assert!(results[0].passes.iter().all(|p| p.disk_busy.is_empty()));
}

/// Catches counts by method that are the proxy's totals instead of the pass's share, that mix
/// the requests of the pass with what came after it, that list a method with no request, or that
/// are written for an arm with no proxy: before the pass the proxy had seen 1 GET; in the first
/// pass it forwarded 4 HEAD and 2 PUT, and during its cleanup hook 3 DELETE; in the second 6 HEAD and, then,
/// 2 DELETE and 1 GET.
#[tokio::test(start_paused = true)]
async fn a_pass_records_what_the_proxy_forwarded_by_method_during_it_and_after_it() {
    let dir = temp("tb-engine-methods");
    let env = env_with(dir.path(), Script::calm());
    let proxy = CountedProxy::of(&[
        &[("GET", 1)],
        &[("GET", 1), ("HEAD", 4), ("PUT", 2)],
        &[("DELETE", 3), ("GET", 1), ("HEAD", 4), ("PUT", 2)],
        &[("DELETE", 3), ("GET", 1), ("HEAD", 4), ("PUT", 2)],
        &[("DELETE", 3), ("GET", 1), ("HEAD", 10), ("PUT", 2)],
        &[("DELETE", 5), ("GET", 2), ("HEAD", 10), ("PUT", 2)],
    ]);
    let arms = vec![
        arm("proxied", 1, 1, 1, Some(proxy)),
        arm("direct", 1, 1, 1, None),
    ];
    let results = run_cell(&env, CellId::A1, arms, 2, None).await.unwrap();
    let by = |arm: &str| results.iter().find(|r| r.arm == arm).unwrap();
    let proxied = &by("proxied").passes;
    let first = proxied[0].proxy_requests.as_ref().expect("counts");
    assert_eq!(first.during, counts(&[("HEAD", 4), ("PUT", 2)]));
    assert_eq!(first.cleanup, counts(&[("DELETE", 3)]));
    let second = proxied[1].proxy_requests.as_ref().expect("counts");
    assert_eq!(second.during, counts(&[("HEAD", 6)]));
    assert_eq!(second.cleanup, counts(&[("DELETE", 2), ("GET", 1)]));
    assert!(
        by("direct")
            .passes
            .iter()
            .all(|p| p.proxy_requests.is_none())
    );
}

/// Catches a pass that does not say how many requests it had in flight at most, and a width that
/// is taken for the asked one: three workers that all wait for each other are three in flight,
/// and one worker is one however many requests it sends.
#[tokio::test(start_paused = true)]
async fn a_pass_records_the_most_requests_it_had_in_flight() {
    let dir = temp("tb-engine-peak");
    let env = env_with(dir.path(), Script::calm());
    let gate = Arc::new(Barrier::new(3));
    let mut wide = arm("wide", 3, 3, 0, None);
    wide.work = work(move |_| {
        let gate = gate.clone();
        async move {
            gate.wait().await;
            Ok(vec![step()])
        }
    });
    let arms = vec![wide, arm("narrow", 3, 1, 0, None)];
    let results = run_cell(&env, CellId::ColdPut, arms, 2, None)
        .await
        .unwrap();
    let peak = |arm: &str| -> Vec<u32> {
        let r = results.iter().find(|r| r.arm == arm).unwrap();
        r.passes.iter().map(|p| p.peak_in_flight).collect()
    };
    assert_eq!(peak("wide"), [3, 3]);
    assert_eq!(peak("narrow"), [1, 1]);
}

fn read(dir: &Path, file: &str) -> CellResult {
    CellResult::from_json(&std::fs::read_to_string(dir.join(file)).unwrap()).unwrap()
}

/// Catches a result that does not say which request-rate caps the run held its clients to (a
/// cell that waits at a cap measures the cap), in the returned result or in the file: both
/// carry the three caps of the run.
#[tokio::test(start_paused = true)]
async fn every_result_carries_the_rate_caps_of_the_run() {
    let dir = temp("tb-engine-caps");
    let mut ctx = testkit::ctx(dir.path(), &[], Window::Quiet);
    let caps = RateCaps {
        cold_rps: 20,
        hot_rps: 50,
        list_poll_rps: 2,
    };
    ctx.config.rates = caps;
    let env = Env::new(&ctx, Script::calm()).unwrap();
    let arms = vec![arm("a", 1, 1, 1, None), arm("b", 1, 1, 1, None)];
    let results = run_cell(&env, CellId::G, arms, 2, None).await.unwrap();
    assert!(results.iter().all(|r| r.rate_caps == Some(caps)));
    assert_eq!(read(dir.path(), "g-a-a1.json").rate_caps, Some(caps));
    assert_eq!(read(dir.path(), "g-b-a1.json").rate_caps, Some(caps));
}
