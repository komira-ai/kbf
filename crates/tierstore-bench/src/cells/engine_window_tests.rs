//! Tests of the cold store guard's windows in the engine: what it reads at the ends of every
//! pass, what it makes of the readings and what the result says of them. The readings are given
//! by a scripted meter and the harness's own requests are written to the request log by the
//! arms; no socket, no sleep of real time. Compiled for tests only.

use std::path::Path;
use std::sync::Arc;

use kbf_testkit::dir::TestDir;

use crate::cells::CellId;
use crate::cells::coldguard::ColdGuardLimits;
use crate::cells::engine::{Arm, Env, Pace, Step, result_name, run_cell, work};
use crate::cells::testkit::{self, MeterScript, Script, counter};
use crate::coldserver::{ColdServer, decide};
use crate::coldstate::ColdReading;
use crate::error::BenchError;
use crate::requestlog::{RequestLog, RequestRecord};
use crate::result::{CellResult, Op, Window};
use crate::role::Role;

fn temp(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), name)
}

/// An environment with the meter `meter` and the real defaults of the guard (a 30 s backstop,
/// 10 foreign requests); the request log is returned for the arms to write to.
fn env(dir: &Path, meter: Arc<MeterScript>) -> (Env, Arc<RequestLog>) {
    let mut ctx = testkit::ctx(dir, &[], Window::Quiet);
    ctx.config.cold_guard = ColdGuardLimits::default();
    ctx.cold_meter = Some(meter);
    let log = ctx.log.clone();
    (Env::new(&ctx, Script::calm()).unwrap(), log)
}

fn step(role: Role, latency_us: u64) -> Step {
    Step {
        label: "",
        second_quantity: false,
        role,
        op: Op::Put,
        bytes: 10,
        started_unix_ms: 5,
        latency_us,
        status: Some(200),
        ok: true,
        cold_write: 0,
        in_flight: 0,
    }
}

/// An arm of `requests` requests a pass to `role`, each taking `latency_us` and written to `log`
/// as a request to the cold role, as the bench's clients do.
fn arm(log: &Arc<RequestLog>, requests: u64, role: Role, latency_us: u64, faulted: bool) -> Arm {
    let log = log.clone();
    Arm {
        label: "a".to_owned(),
        pace: Pace::Closed,
        in_flight: 1,
        requests,
        cold_bytes_per_pass: 0,
        cold_bytes_once: 0,
        work: work(move |_| {
            let log = log.clone();
            async move {
                log.record(&RequestRecord {
                    unix_ms: 5,
                    role: Role::Cold,
                    method: "PUT".to_owned(),
                    bucket: "kbf-fake".to_owned(),
                    key: "k".to_owned(),
                    status: Some(200),
                    latency_us,
                    bytes: 10,
                    error_kind: None,
                })?;
                Ok(vec![step(role, latency_us)])
            }
        }),
        before: None,
        after: None,
        faulted,
        proxy: None,
        notes: vec![],
    }
}

fn written(dir: &Path) -> CellResult {
    written_of(dir, CellId::ColdPut)
}

fn written_of(dir: &Path, cell: CellId) -> CellResult {
    let text = std::fs::read_to_string(dir.join(result_name(cell, "a", 1))).unwrap();
    CellResult::from_json(&text).unwrap()
}

/// A reading of a store that lists the drives `drives`, with a counter at 1,040.
fn drives(states: &[&str]) -> Result<ColdReading, String> {
    let mut reading = counter(1_040).unwrap();
    reading.state.drives = states
        .iter()
        .enumerate()
        .map(|(i, state)| crate::result::DriveState {
            id: format!("d-{i:08}"),
            state: (*state).to_owned(),
            healing: false,
        })
        .collect();
    Ok(reading)
}

/// Catches the circular guard that stopped a measurement: every request takes 2.5 s (a small
/// write on a set of hard disks), the store is quiet, and the run goes on to the end. Each pass
/// records what was read: 40 requests at the store, 40 sent, none from elsewhere.
#[tokio::test(start_paused = true)]
async fn a_quiet_cold_store_with_slow_requests_of_its_own_is_not_stopped() {
    let dir = temp("tb-window-quiet");
    let meter = MeterScript::of(vec![counter(1_000), counter(1_040), counter(1_080)]);
    let (env, log) = env(dir.path(), meter.clone());
    let results = run_cell(
        &env,
        CellId::ColdPut,
        vec![arm(&log, 40, Role::Cold, 2_500_000, false)],
        2,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        meter.reads(),
        3,
        "once to begin, once at the end of each pass"
    );
    for pass in &results[0].passes {
        assert_eq!(
            (pass.cold_guard.clone(), pass.window),
            (None, Window::Quiet)
        );
        let window = pass.cold_window.clone().unwrap();
        assert_eq!(
            (
                window.minio_requests,
                window.own_requests,
                window.foreign_requests
            ),
            (40, 40, 0)
        );
        assert!(window.judged);
        assert_eq!(
            (window.drives, window.drives_not_ok, window.healing),
            (1, 0, false)
        );
    }
}

/// Catches requests from elsewhere not stopping the run: 25 more requests at the store than the
/// bench sent in the second pass is over 10. The run ends with the trip, which names the count
/// and the limit; the pass is cut and marked busy, its readings are in the result, and the first
/// pass is whole and quiet.
#[tokio::test(start_paused = true)]
async fn requests_from_elsewhere_stop_the_run_and_the_result_says_so() {
    let dir = temp("tb-window-foreign");
    let meter = MeterScript::of(vec![counter(1_000), counter(1_040), counter(1_105)]);
    let (env, log) = env(dir.path(), meter);
    let err = run_cell(
        &env,
        CellId::ColdPut,
        vec![arm(&log, 40, Role::Cold, 20_000, false)],
        4,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, BenchError::ColdGuard(_)), "{err}");
    let text = err.to_string();
    assert!(text.contains("foreign_requests is 25"), "{text}");
    assert!(text.contains("limit 10"), "{text}");
    let result = written(dir.path());
    assert_eq!(result.passes.len(), 2, "the run ended with the second pass");
    assert_eq!(result.passes[0].cold_guard, None);
    assert_eq!(result.passes[0].window, Window::Quiet);
    let cut = &result.passes[1];
    assert_eq!(cut.cold_guard.clone().unwrap().to_string(), text);
    assert_eq!(cut.window, Window::Busy);
    let window = cut.cold_window.clone().unwrap();
    assert_eq!(
        (
            window.minio_requests,
            window.own_requests,
            window.foreign_requests
        ),
        (65, 40, 25)
    );
}

/// Catches a store that cannot be read at the start being run on: the run ends before a request
/// is sent, with the reason in the trip, and the (empty) pass is in the result.
#[tokio::test(start_paused = true)]
async fn a_store_that_cannot_be_read_at_the_start_ends_the_run_before_a_request() {
    let dir = temp("tb-window-unreadable-start");
    let meter = MeterScript::of(vec![Err("no answer in 30 s".to_owned())]);
    let (env, log) = env(dir.path(), meter);
    let err = run_cell(
        &env,
        CellId::ColdPut,
        vec![arm(&log, 40, Role::Cold, 20_000, false)],
        2,
        None,
    )
    .await
    .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("unreadable is"), "{text}");
    assert!(text.contains("no answer in 30 s"), "{text}");
    let result = written(dir.path());
    assert_eq!(result.passes.len(), 1);
    assert_eq!(result.passes[0].requests, 0);
    assert_eq!(result.passes[0].window, Window::Busy);
    assert_eq!(log.count(), 0, "nothing was sent");
}

/// Catches a counter that cannot be read at the end of a pass being taken for a quiet window:
/// the run ends, the pass that was sent is in the result with its trip and no readings.
#[tokio::test(start_paused = true)]
async fn a_counter_that_cannot_be_read_at_the_end_of_a_pass_stops_the_run() {
    let dir = temp("tb-window-unreadable-end");
    let meter = MeterScript::of(vec![counter(1_000), Err("timed out".to_owned())]);
    let (env, log) = env(dir.path(), meter);
    let err = run_cell(
        &env,
        CellId::ColdPut,
        vec![arm(&log, 40, Role::Cold, 20_000, false)],
        3,
        None,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("timed out"), "{err}");
    let result = written(dir.path());
    assert_eq!(result.passes.len(), 1);
    assert_eq!(result.passes[0].requests, 40);
    assert_eq!(result.passes[0].window, Window::Busy);
    assert_eq!(result.passes[0].cold_window, None);
    assert_eq!(
        result.passes[0].cold_guard.clone().unwrap().metric,
        "unreadable"
    );
}

/// Catches a drive that goes offline during a pass not stopping the run: the trip names the
/// drives and the result keeps the readings.
#[tokio::test(start_paused = true)]
async fn a_drive_that_is_not_ok_at_the_end_of_a_pass_stops_the_run() {
    let dir = temp("tb-window-drive");
    let meter = MeterScript::of(vec![counter(1_000), drives(&["ok", "offline"])]);
    let (env, log) = env(dir.path(), meter);
    let err = run_cell(
        &env,
        CellId::ColdPut,
        vec![arm(&log, 40, Role::Cold, 20_000, false)],
        3,
        None,
    )
    .await
    .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("drives is 1"), "{text}");
    let pass = &written(dir.path()).passes[0];
    let window = pass.cold_window.clone().unwrap();
    assert_eq!((window.drives, window.drives_not_ok), (2, 1));
    assert_eq!(pass.window, Window::Busy);
}

/// Catches foreign requests judged where the bench cannot tell them from the instance's own
/// copies: a pass of an arm through the fault proxy, and one with a request to another role, are
/// recorded with `judged` false and the run finishes, 500 requests from elsewhere or not.
#[tokio::test(start_paused = true)]
async fn passes_the_bench_cannot_attribute_are_recorded_and_not_judged() {
    for (name, role, faulted) in [
        ("tb-window-faulted", Role::Cold, true),
        ("tb-window-instance", Role::Instance, false),
    ] {
        let dir = temp(name);
        let meter = MeterScript::of(vec![counter(1_000), counter(1_540)]);
        let (env, log) = env(dir.path(), meter);
        let results = run_cell(
            &env,
            CellId::ColdPut,
            vec![arm(&log, 40, role, 20_000, faulted)],
            1,
            None,
        )
        .await
        .unwrap();
        let window = results[0].passes[0].cold_window.clone().unwrap();
        assert_eq!(
            (window.foreign_requests, window.judged),
            (500, false),
            "{name}"
        );
        assert!(window.unjudged_reason.is_some(), "{name}");
    }
}

/// Catches the backstop being a health limit again, and a pass cut by the guard's own window
/// reading the counter after the run has ended: requests of 31 s are a hung store, the trip
/// says so, and only the first reading was made.
#[tokio::test(start_paused = true)]
async fn a_hung_cold_store_trips_the_backstop_and_reads_no_counter_after_it() {
    let dir = temp("tb-window-backstop");
    let meter = MeterScript::of(vec![counter(1_000)]);
    let (env, log) = env(dir.path(), meter.clone());
    let err = run_cell(
        &env,
        CellId::ColdPut,
        vec![arm(&log, 40, Role::Cold, 31_000_000, false)],
        2,
        None,
    )
    .await
    .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("p99_ms is 31000"), "{text}");
    assert!(text.contains("hung-store backstop"), "{text}");
    assert_eq!(meter.reads(), 1);
    assert_eq!(written(dir.path()).passes[0].cold_window, None);
}

/// An environment whose stores are `stores` and whose cold cluster is `server`.
fn env_on(
    dir: &Path,
    meter: Arc<MeterScript>,
    stores: &[(Role, &str)],
    server: ColdServer,
) -> (Env, Arc<RequestLog>) {
    let mut ctx = testkit::ctx(dir, stores, Window::Quiet);
    ctx.config.cold_guard = ColdGuardLimits::default();
    ctx.cold_meter = Some(meter);
    ctx.cold_server = server;
    let log = ctx.log.clone();
    (Env::new(&ctx, Script::calm()).unwrap(), log)
}

/// An arm of `requests` requests to the hot store, each written to `log` as a request of the hot
/// role.
fn hot_arm(log: &Arc<RequestLog>, requests: u64) -> Arm {
    let mut hot = arm(log, requests, Role::Hot, 20_000, false);
    let log = log.clone();
    hot.work = work(move |_| {
        let log = log.clone();
        async move {
            log.record(&RequestRecord {
                unix_ms: 5,
                role: Role::Hot,
                method: "PUT".to_owned(),
                bucket: "kbf-fake".to_owned(),
                key: "k".to_owned(),
                status: Some(200),
                latency_us: 20_000,
                bytes: 10,
                error_kind: None,
            })?;
            Ok(vec![step(Role::Hot, 20_000)])
        }
    });
    hot
}

const STORES: [(Role, &str); 2] = [
    (Role::Cold, "http://127.0.0.1:30900"),
    (Role::Hot, "http://192.0.2.5:30900"),
];

/// Catches the false trip of a hot-only pass on its own traffic: the hot store is the same
/// cluster as the cold one at another address (the same deployment id), so its 50 requests are in
/// the cluster's counter and in the harness's count, and none is foreign.
#[tokio::test(start_paused = true)]
async fn a_hot_pass_on_the_cold_cluster_is_not_foreign() {
    let dir = temp("tb-window-hot-same");
    let meter = MeterScript::of(vec![counter(1_000), counter(1_050), counter(1_100)]);
    let server = decide(true, None, Some(("dep-1", "dep-1")));
    let (env, log) = env_on(dir.path(), meter, &STORES, server);
    let results = run_cell(&env, CellId::A1, vec![hot_arm(&log, 50)], 2, None)
        .await
        .unwrap();
    for pass in &results[0].passes {
        let window = pass.cold_window.clone().unwrap();
        assert_eq!(
            (
                window.minio_requests,
                window.own_requests,
                window.foreign_requests
            ),
            (50, 50, 0)
        );
        assert!(window.judged);
    }
}

/// Catches a hot store on another cluster being counted as the harness's own: its requests are in
/// neither the counter nor the count, and load from elsewhere on the cold cluster still stops the
/// run.
#[tokio::test(start_paused = true)]
async fn a_hot_pass_on_another_cluster_is_not_counted_and_foreign_load_still_trips() {
    let dir = temp("tb-window-hot-other");
    let meter = MeterScript::of(vec![counter(1_000), counter(1_000), counter(1_025)]);
    let server = decide(true, None, Some(("dep-1", "dep-2")));
    let (env, log) = env_on(dir.path(), meter, &STORES, server);
    let err = run_cell(&env, CellId::A1, vec![hot_arm(&log, 50)], 3, None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("foreign_requests is 25"), "{err}");
    let result = written_of(dir.path(), CellId::A1);
    let first = result.passes[0].cold_window.clone().unwrap();
    assert_eq!(
        (first.own_requests, first.foreign_requests, first.judged),
        (0, 0, true)
    );
}

/// Catches a guess about the hot store's cluster: nothing decides it, so the hot pass is
/// recorded with the reason and not judged, however much the counter grew.
#[tokio::test(start_paused = true)]
async fn a_hot_pass_whose_cluster_is_undecided_is_not_judged_and_says_why() {
    let dir = temp("tb-window-hot-undecided");
    let meter = MeterScript::of(vec![counter(1_000), counter(1_540)]);
    let (env, log) = env_on(dir.path(), meter, &STORES, decide(true, None, None));
    let results = run_cell(&env, CellId::A1, vec![hot_arm(&log, 40)], 1, None)
        .await
        .unwrap();
    let window = results[0].passes[0].cold_window.clone().unwrap();
    assert_eq!((window.foreign_requests, window.judged), (540, false));
    let reason = window.unjudged_reason.unwrap();
    assert!(reason.contains("could not be told apart"), "{reason}");
}
