//! Tests of what the engine keeps of a run: result files per attempt that are never written
//! over, the cold store's state read before every pass, and the cold store guard that ends a run
//! when the store's own health turns bad. Compiled for tests only.
//!
//! The guard tests run on a paused clock with a fake cold store that turns slow or erroring after
//! a chosen request; no socket, no sleep of real time.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kbf_testkit::dir::TestDir;

use crate::cells::CellId;
use crate::cells::engine::{Arm, Env, Pace, Step, hook, result_name, run_cell, work, write_new};
use crate::cells::testkit::{self, ColdScript, Script, cold_state};
use crate::coldstate::ColdStateSource;
use crate::error::BenchError;
use crate::result::{CellResult, Op, Window};
use crate::role::Role;

fn temp(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), name)
}

/// The `attempt`-th try of a run whose results are in `dir`, asking `cold` about the cold store.
fn env(dir: &Path, attempt: u32, cold: Arc<dyn ColdStateSource>) -> Env {
    let mut ctx = testkit::ctx(dir, &[], Window::Quiet);
    ctx.provenance.attempt = attempt;
    ctx.cold_state = cold;
    Env::new(&ctx, Script::calm()).unwrap()
}

fn no_cold() -> Arc<dyn ColdStateSource> {
    Arc::new(crate::coldstate::NoColdState)
}

fn step(role: Role, latency_us: u64, ok: bool) -> Step {
    Step {
        label: "",
        second_quantity: false,
        role,
        op: Op::Put,
        bytes: 10,
        started_unix_ms: 5,
        latency_us,
        status: Some(if ok { 200 } else { 500 }),
        ok,
        cold_write: 0,
        in_flight: 0,
    }
}

/// An arm of `requests` fast cold requests a pass, counting them in `sent`.
fn arm(label: &str, requests: u64, sent: &Arc<AtomicU64>) -> Arm {
    let sent = sent.clone();
    Arm {
        label: label.to_owned(),
        pace: Pace::Closed,
        in_flight: 1,
        requests,
        cold_bytes_per_pass: 0,
        cold_bytes_once: 0,
        work: work(move |_| {
            let sent = sent.clone();
            async move {
                sent.fetch_add(1, Ordering::SeqCst);
                Ok(vec![step(Role::Cold, 1_000, true)])
            }
        }),
        before: None,
        after: None,
        faulted: false,
        proxy: None,
        notes: vec![],
    }
}

fn written(dir: &Path, cell: CellId, arm: &str, attempt: u32) -> CellResult {
    let text = std::fs::read_to_string(dir.join(result_name(cell, arm, attempt))).unwrap();
    CellResult::from_json(&text).unwrap()
}

fn csv_attempts(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("samples.csv"))
        .unwrap()
        .lines()
        .skip(1)
        .map(|l| l.rsplit(',').next().unwrap().to_owned())
        .collect()
}

/// Catches a re-run of a cell and arm that replaces the result of the first: the second run is
/// refused before it sends one request, says how to keep both, and the first result and its
/// samples are as they were.
#[tokio::test]
async fn a_rerun_of_the_same_cell_and_arm_is_refused_before_it_sends() {
    let dir = temp("tb-gaps-rerun");
    let sent = Arc::new(AtomicU64::new(0));
    let first = run_cell(
        &env(dir.path(), 1, no_cold()),
        CellId::ColdPut,
        vec![arm("a", 3, &sent)],
        2,
        None,
    )
    .await
    .unwrap();
    assert_eq!(sent.load(Ordering::SeqCst), 6);
    assert!(first[0].cold_state.is_empty(), "no cold store, no reading");
    let path = dir.path().join("cold-put-a-a1.json");
    let before = std::fs::read(&path).unwrap();
    let rows = csv_attempts(dir.path()).len();

    let err = run_cell(
        &env(dir.path(), 1, no_cold()),
        CellId::ColdPut,
        vec![arm("a", 3, &sent)],
        2,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, BenchError::ResultExists { .. }), "{err}");
    let expected = BenchError::ResultExists { path: path.clone() };
    assert_eq!(err.to_string(), expected.to_string());
    assert!(err.to_string().contains("--attempt"), "{err}");
    assert_eq!(sent.load(Ordering::SeqCst), 6, "nothing was sent");
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(csv_attempts(dir.path()).len(), rows);
}

/// Catches a retry that cannot be kept: attempt 2 writes `-a2` beside the first result, every
/// sample row names its attempt, and the attempts carry their number in their provenance.
#[tokio::test]
async fn a_retry_under_the_next_attempt_keeps_every_attempt() {
    let dir = temp("tb-gaps-retry");
    let sent = Arc::new(AtomicU64::new(0));
    for attempt in [1, 2] {
        run_cell(
            &env(dir.path(), attempt, no_cold()),
            CellId::ColdPut,
            vec![arm("a", 1, &sent)],
            2,
            None,
        )
        .await
        .unwrap();
    }
    assert_eq!(
        written(dir.path(), CellId::ColdPut, "a", 1)
            .provenance
            .attempt,
        1
    );
    assert_eq!(
        written(dir.path(), CellId::ColdPut, "a", 2)
            .provenance
            .attempt,
        2
    );
    assert_eq!(csv_attempts(dir.path()), ["1", "1", "2", "2"]);
    assert_eq!(result_name(CellId::ColdPut, "a", 2), "cold-put-a-a2.json");
}

/// Catches a check for an existing result that looks at the wrong names: an arm that only
/// shares a prefix with a result (`a2` beside `a`), or a result of another attempt, must not
/// stop a run; a result of a step suffix (`a.ttfb`) must, before anything is sent.
#[tokio::test]
async fn only_a_result_of_this_arm_and_attempt_stops_the_run() {
    let dir = temp("tb-gaps-names");
    let sent = Arc::new(AtomicU64::new(0));
    for name in [
        "cold-read-a2-a1.json",
        "cold-read-a-a2.json",
        "cold-read-a-a1.txt",
    ] {
        std::fs::write(dir.path().join(name), "{}").unwrap();
    }
    let env1 = env(dir.path(), 1, no_cold());
    run_cell(&env1, CellId::ColdRead, vec![arm("a", 1, &sent)], 2, None)
        .await
        .unwrap();
    let before = sent.load(Ordering::SeqCst);

    std::fs::remove_file(dir.path().join("cold-read-a-a1.json")).unwrap();
    std::fs::write(dir.path().join("cold-read-a.ttfb-a1.json"), "{}").unwrap();
    let err = run_cell(
        &env(dir.path(), 1, no_cold()),
        CellId::ColdRead,
        vec![arm("a", 1, &sent)],
        2,
        None,
    )
    .await
    .unwrap_err();
    let expected = BenchError::ResultExists {
        path: dir.path().join("cold-read-a.ttfb-a1.json"),
    };
    assert_eq!(err.to_string(), expected.to_string());
    assert_eq!(sent.load(Ordering::SeqCst), before);
}

/// Catches a result file that appears after the run began (another run, a hand) being replaced
/// at the end: the file is made exclusively, the run says so, and the other file is as it was.
#[tokio::test]
async fn a_result_that_appears_during_the_run_is_not_replaced() {
    let dir = temp("tb-gaps-race");
    let sent = Arc::new(AtomicU64::new(0));
    let target = dir.path().join("cold-put-a-a1.json");
    let mut a = arm("a", 1, &sent);
    let planted = target.clone();
    a.after = Some(hook(move |_| {
        let planted = planted.clone();
        async move {
            std::fs::write(&planted, "someone else's result").unwrap();
            Ok(vec![])
        }
    }));
    let err = run_cell(
        &env(dir.path(), 1, no_cold()),
        CellId::ColdPut,
        vec![a],
        2,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, BenchError::ResultExists { .. }), "{err}");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "someone else's result"
    );
}

/// Catches `write_new` replacing a file, or naming a directory that is not there as a file that
/// exists.
#[test]
fn write_new_makes_a_file_and_never_replaces_one() {
    let dir = temp("tb-gaps-write-new");
    let path = dir.path().join("r.json");
    write_new(&path, "one").unwrap();
    assert!(matches!(
        write_new(&path, "two"),
        Err(BenchError::ResultExists { .. })
    ));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "one");
    assert!(matches!(
        write_new(&dir.path().join("absent").join("r.json"), "x"),
        Err(BenchError::Io { .. })
    ));
}

/// Catches a cold state read once instead of before every pass, one lost between arms, and one
/// stamped into the wrong arm or pass: two arms, three passes, six readings, each result holds
/// the reading taken before each of its own passes (healing flag, drives and time included).
#[tokio::test]
async fn the_cold_state_is_read_before_every_pass_and_stamped_in_each_result() {
    let dir = temp("tb-gaps-cold-state");
    // Five answers for six reads: the sixth is the fifth again.
    let healing = [false, false, true, false, true];
    let script = ColdScript::of(
        healing
            .iter()
            .enumerate()
            .map(|(i, h)| Ok(cold_state(*h, 100 + i as u64)))
            .collect(),
    );
    let sent = Arc::new(AtomicU64::new(0));
    let results = run_cell(
        &env(dir.path(), 1, script.clone()),
        CellId::G,
        vec![arm("a", 1, &sent), arm("b", 1, &sent)],
        3,
        None,
    )
    .await
    .unwrap();
    assert_eq!(script.reads(), 6);
    let of = |r: &CellResult| -> Vec<(u32, bool, u64)> {
        r.cold_state
            .iter()
            .map(|c| (c.pass, c.state.healing, c.state.read_unix_ms))
            .collect()
    };
    // Pass 0 asks for a then b, pass 1 for b then a, pass 2 for a then b.
    assert_eq!(
        of(&results[0]),
        [(0, false, 100), (1, false, 103), (2, true, 104)]
    );
    assert_eq!(
        of(&results[1]),
        [(0, false, 101), (1, true, 102), (2, true, 104)]
    );
    assert_eq!(results[0].cold_state[2].state.drives[0].id, "d-00000000");
    assert_eq!(results[1].cold_state[1].state.percent_done, Some(40.0));
    // And it is in the file.
    assert_eq!(written(dir.path(), CellId::G, "b", 1), results[1]);
}

/// Catches a reading that cannot be made being ignored: the run stops before the pass it was to
/// precede, having sent nothing in it.
#[tokio::test]
async fn a_cold_state_that_cannot_be_read_stops_the_run_before_the_pass() {
    let dir = temp("tb-gaps-cold-state-fails");
    let script = ColdScript::of(vec![Ok(cold_state(false, 1)), Err(())]);
    let sent = Arc::new(AtomicU64::new(0));
    let err = run_cell(
        &env(dir.path(), 1, script.clone()),
        CellId::G,
        vec![arm("a", 4, &sent)],
        3,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, BenchError::NotYet("cold state")), "{err}");
    assert_eq!(script.reads(), 2);
    assert_eq!(sent.load(Ordering::SeqCst), 4, "only pass 0 was sent");
}

/// How a fake cold store goes bad.
#[derive(Clone, Copy)]
enum Turn {
    /// Every request takes 3 s.
    Slow,
    /// Every request fails at once.
    Erroring,
}

/// A cold store that is healthy until request `from_index` of pass `from_pass`, then `turn`s.
/// Every request takes 100 ms of the paused clock.
#[derive(Clone, Copy)]
struct FakeStore {
    from_pass: u32,
    from_index: u64,
    turn: Turn,
    role: Role,
}

impl FakeStore {
    fn arm(self, label: &str, requests: u64, faulted: bool) -> Arm {
        let mut arm = arm(label, requests, &Arc::new(AtomicU64::new(0)));
        arm.work = work(move |unit| async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let bad = (unit.pass, unit.index) >= (self.from_pass, self.from_index);
            let step = match (bad, self.turn) {
                (false, _) => step(self.role, 20_000, true),
                (true, Turn::Slow) => step(self.role, 3_000_000, true),
                (true, Turn::Erroring) => step(self.role, 1_000, false),
            };
            Ok(vec![step])
        });
        arm.faulted = faulted;
        arm
    }
}

fn store(from_pass: u32, from_index: u64, turn: Turn) -> FakeStore {
    FakeStore {
        from_pass,
        from_index,
        turn,
        role: Role::Cold,
    }
}

/// Catches a guard that does not stop the run when the cold store turns slow: the p99 of the
/// window goes over 2,000 ms at the first slow request after 30 good ones; the run ends with the
/// trip, the pass is cut and marked busy though the run said quiet, the trip is in the result,
/// and the file is written.
#[tokio::test(start_paused = true)]
async fn a_cold_store_that_turns_slow_stops_the_run_and_the_trip_is_recorded() {
    let dir = temp("tb-gaps-guard-slow");
    let err = run_cell(
        &env(dir.path(), 1, no_cold()),
        CellId::ColdPut,
        vec![store(0, 30, Turn::Slow).arm("a", 200, false)],
        3,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, BenchError::ColdGuard(_)), "{err}");
    let result = written(dir.path(), CellId::ColdPut, "a", 1);
    assert_eq!(result.passes.len(), 1, "the run ended with the pass");
    let pass = &result.passes[0];
    let trip = pass.cold_guard.clone().unwrap();
    assert_eq!(err.to_string(), trip.to_string());
    assert_eq!(trip.metric, "p99_ms");
    assert!((trip.value - 3_000.0).abs() < 1e-9);
    assert!((trip.limit - 2_000.0).abs() < 1e-9);
    assert_eq!((trip.requests, trip.window_secs), (31, 60));
    assert_eq!(
        (pass.window, pass.gate_tripped.as_deref()),
        (Window::Busy, None)
    );
    assert!((31..=33).contains(&pass.requests), "{}", pass.requests);
}

/// Catches a guard that does not stop the run when the cold store starts to fail: the second
/// failure after 30 good requests is 2 of 32, over 5%; one failure alone (1 of 31) is not.
#[tokio::test(start_paused = true)]
async fn a_cold_store_that_starts_to_fail_stops_the_run_at_the_error_share() {
    let dir = temp("tb-gaps-guard-errors");
    let err = run_cell(
        &env(dir.path(), 1, no_cold()),
        CellId::ColdPut,
        vec![store(0, 30, Turn::Erroring).arm("a", 200, false)],
        3,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, BenchError::ColdGuard(_)), "{err}");
    let result = written(dir.path(), CellId::ColdPut, "a", 1);
    let pass = &result.passes[0];
    let trip = pass.cold_guard.clone().unwrap();
    assert_eq!(err.to_string(), trip.to_string());
    assert_eq!(trip.metric, "error_share");
    assert!((trip.value - 2.0 / 32.0).abs() < 1e-9, "{trip}");
    assert_eq!(trip.requests, 32);
    assert!(pass.failures >= 2);
}

/// Catches a window that is reset by a new pass: a store that turns slow in the second pass is
/// seen against the requests of the first, the first pass is whole and unmarked, and the run
/// ends in the second.
#[tokio::test(start_paused = true)]
async fn the_window_carries_over_from_one_pass_to_the_next() {
    let dir = temp("tb-gaps-guard-carry");
    let err = run_cell(
        &env(dir.path(), 1, no_cold()),
        CellId::ColdPut,
        vec![store(1, 30, Turn::Slow).arm("a", 50, false)],
        4,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, BenchError::ColdGuard(_)), "{err}");
    let result = written(dir.path(), CellId::ColdPut, "a", 1);
    assert_eq!(result.passes.len(), 2);
    assert_eq!(result.passes[0].cold_guard, None);
    assert_eq!(result.passes[0].window, Window::Quiet);
    assert_eq!(result.passes[0].requests, 50);
    let trip = result.passes[1].cold_guard.clone().unwrap();
    assert_eq!(err.to_string(), trip.to_string());
    assert_eq!(trip.requests, 50 + 31);
}

/// Catches a guard that judges what it was not meant to: an arm that puts the cold store in a
/// fault on purpose (its requests are slow by design), and requests to a store that is not the
/// cold role, are not shown to it; both runs finish all their passes.
#[tokio::test(start_paused = true)]
async fn a_faulted_arm_and_other_roles_are_not_judged() {
    let dir = temp("tb-gaps-guard-exempt");
    let results = run_cell(
        &env(dir.path(), 1, no_cold()),
        CellId::ColdPut,
        vec![store(0, 0, Turn::Slow).arm("faulted", 40, true)],
        2,
        None,
    )
    .await
    .unwrap();
    assert_eq!(results[0].passes.len(), 2);
    assert!(results[0].passes.iter().all(|p| p.cold_guard.is_none()));

    let instance = FakeStore {
        role: Role::Instance,
        ..store(0, 0, Turn::Erroring)
    };
    let results = run_cell(
        &env(dir.path(), 2, no_cold()),
        CellId::ColdPut,
        vec![instance.arm("instance", 40, false)],
        2,
        None,
    )
    .await
    .unwrap();
    assert_eq!(results[0].passes.len(), 2);
    assert!(results[0].passes.iter().all(|p| p.cold_guard.is_none()));
}

/// Catches a guard that stops a healthy run, and the same slow store judged when the arm is
/// not marked as faulted (the control of the exemption above).
#[tokio::test(start_paused = true)]
async fn a_healthy_cold_store_finishes_and_the_same_slow_one_does_not() {
    let dir = temp("tb-gaps-guard-control");
    let healthy = store(u32::MAX, 0, Turn::Slow);
    let results = run_cell(
        &env(dir.path(), 1, no_cold()),
        CellId::ColdPut,
        vec![healthy.arm("a", 40, false)],
        3,
        None,
    )
    .await
    .unwrap();
    assert_eq!(results[0].passes.len(), 3);
    let err = run_cell(
        &env(dir.path(), 2, no_cold()),
        CellId::ColdPut,
        vec![store(0, 0, Turn::Slow).arm("a", 40, false)],
        3,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, BenchError::ColdGuard(_)), "{err}");
}

/// A streamed cold read as the cold-read cell reports it: a read that succeeded is two steps,
/// the whole request and the same request's time to the first byte (`.ttfb`); a read that
/// failed is the whole request alone. `shape` says, of request `index`, whether it succeeded
/// and how long it took.
fn streamed_reads(
    label: &str,
    requests: u64,
    shape: impl Fn(u64) -> (bool, u64) + Send + Sync + 'static,
) -> Arm {
    let mut arm = arm(label, requests, &Arc::new(AtomicU64::new(0)));
    arm.work = work(move |unit| {
        let (ok, latency_us) = shape(unit.index);
        async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let mut steps = vec![step(Role::Cold, latency_us, ok)];
            if ok {
                steps.push(Step {
                    label: ".ttfb",
                    second_quantity: true,
                    ..step(Role::Cold, 1_000, true)
                });
            }
            Ok(steps)
        }
    });
    arm
}

/// Catches a guard that counts a streamed read twice when it succeeded and once when it failed
/// (its `.ttfb` step counted as a request): 54 good reads and then failures, the third failure
/// is 3 of 57 requests, over 5%, and the guard says so; counting the pairs it would be 3 of 111
/// and trip only at the sixth failure, 6 of 114 entries.
#[tokio::test(start_paused = true)]
async fn a_streamed_read_counts_once_for_the_error_share() {
    let dir = temp("tb-gaps-guard-streamed-share");
    let err = run_cell(
        &env(dir.path(), 1, no_cold()),
        CellId::ColdRead,
        vec![streamed_reads("a", 60, |i| (i < 54, 20_000))],
        3,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, BenchError::ColdGuard(_)), "{err}");
    let result = written(dir.path(), CellId::ColdRead, "a", 1);
    let trip = result.passes[0].cold_guard.clone().unwrap();
    assert_eq!(trip.metric, "error_share");
    assert_eq!(trip.requests, 57);
    assert!((trip.value - 3.0 / 57.0).abs() < 1e-9, "{trip}");
}

/// Catches reads judged before the window has enough of them: 15 slow successful reads are 15
/// requests, fewer than the 20 the guard needs to judge, though they are 30 steps; counted by
/// step, their p99 of 3 s would stop the run.
#[tokio::test(start_paused = true)]
async fn successful_streamed_reads_are_not_counted_twice_toward_the_minimum() {
    let dir = temp("tb-gaps-guard-streamed-min");
    let results = run_cell(
        &env(dir.path(), 1, no_cold()),
        CellId::ColdRead,
        vec![streamed_reads("a", 15, |_| (true, 3_000_000))],
        1,
        None,
    )
    .await
    .unwrap();
    assert_eq!(results[0].passes.len(), 1);
    assert!(results[0].passes[0].cold_guard.is_none());
}

/// Catches a p99 that takes the time to the first byte as a request: 50 reads, the last of them
/// slow. The p99 of their 50 whole-request latencies is that slowest one and trips; with the 50
/// first-byte times in the window as well, the slow one would be the 100th of 100 and the p99
/// the one before it.
#[tokio::test(start_paused = true)]
async fn the_p99_is_taken_over_whole_requests_only() {
    let dir = temp("tb-gaps-guard-streamed-p99");
    let err = run_cell(
        &env(dir.path(), 1, no_cold()),
        CellId::ColdRead,
        vec![streamed_reads("a", 50, |i| {
            (true, if i == 49 { 3_000_000 } else { 20_000 })
        })],
        3,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, BenchError::ColdGuard(_)), "{err}");
    let result = written(dir.path(), CellId::ColdRead, "a", 1);
    let trip = result.passes[0].cold_guard.clone().unwrap();
    assert_eq!(trip.metric, "p99_ms");
    assert_eq!(trip.requests, 50);
    assert!((trip.value - 3_000.0).abs() < 1e-9, "{trip}");
}

/// A read of cell B as it reports it: the whole direct read of the cold bucket (`.cold-direct`,
/// a request to the cold store), then the first read through the instance with its time to the
/// first byte (`.ttfb`) and the second read (`.second`), which are requests to the instance.
/// From request `from_index` on, the direct read turns `turn`.
fn cell_b_reads(label: &str, requests: u64, from_index: u64, turn: Turn) -> Arm {
    let mut arm = arm(label, requests, &Arc::new(AtomicU64::new(0)));
    arm.work = work(move |unit| async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let direct = match (unit.index >= from_index, turn) {
            (false, _) => step(Role::Cold, 20_000, true),
            (true, Turn::Slow) => step(Role::Cold, 3_000_000, true),
            (true, Turn::Erroring) => step(Role::Cold, 1_000, false),
        };
        Ok(vec![
            Step {
                label: ".cold-direct",
                ..direct
            },
            Step {
                label: ".ttfb",
                second_quantity: true,
                ..step(Role::Instance, 1_000, true)
            },
            step(Role::Instance, 20_000, true),
            Step {
                label: ".second",
                ..step(Role::Instance, 20_000, true)
            },
        ])
    });
    arm
}

/// Catches direct reads of the cold bucket that the guard does not see because they carry a
/// label: cell B's `.cold-direct` reads fail from the 31st, and the second failure is 2 of 32
/// cold requests, over 5%. The instance's steps are not cold requests and do not count.
#[tokio::test(start_paused = true)]
async fn cell_bs_direct_reads_that_fail_trip_the_error_share() {
    let dir = temp("tb-gaps-guard-b-errors");
    let err = run_cell(
        &env(dir.path(), 1, no_cold()),
        CellId::B,
        vec![cell_b_reads("a", 200, 30, Turn::Erroring)],
        3,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, BenchError::ColdGuard(_)), "{err}");
    let result = written(dir.path(), CellId::B, "a", 1);
    let trip = result.passes[0].cold_guard.clone().unwrap();
    assert_eq!(trip.metric, "error_share");
    assert_eq!(trip.requests, 32);
    assert!((trip.value - 2.0 / 32.0).abs() < 1e-9, "{trip}");
}

/// Catches the same blindness for latency: cell B's direct reads turn to 3 s from the 31st and
/// the p99 of the window is that, over 2,000 ms.
#[tokio::test(start_paused = true)]
async fn cell_bs_direct_reads_that_turn_slow_trip_the_p99() {
    let dir = temp("tb-gaps-guard-b-slow");
    let err = run_cell(
        &env(dir.path(), 1, no_cold()),
        CellId::B,
        vec![cell_b_reads("a", 200, 30, Turn::Slow)],
        3,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, BenchError::ColdGuard(_)), "{err}");
    let result = written(dir.path(), CellId::B, "a", 1);
    let trip = result.passes[0].cold_guard.clone().unwrap();
    assert_eq!(trip.metric, "p99_ms");
    assert_eq!(trip.requests, 31);
    assert!((trip.value - 3_000.0).abs() < 1e-9, "{trip}");
}

/// Catches a fix that counts every labelled step: cold requests of cell B that all succeed,
/// 15 of them with a first-byte pair each, are 15 requests, fewer than the 20 the guard needs
/// to judge, though they are 30 cold steps and 15 more instance steps; their 3 s would stop the run.
#[tokio::test(start_paused = true)]
async fn direct_reads_with_a_first_byte_pair_still_count_once() {
    let dir = temp("tb-gaps-guard-b-pair");
    let mut arm = arm("a", 15, &Arc::new(AtomicU64::new(0)));
    arm.work = work(|_| async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        Ok(vec![
            Step {
                label: ".cold-direct",
                ..step(Role::Cold, 3_000_000, true)
            },
            Step {
                label: ".ttfb",
                second_quantity: true,
                ..step(Role::Cold, 1_000, true)
            },
        ])
    });
    let results = run_cell(
        &env(dir.path(), 1, no_cold()),
        CellId::B,
        vec![arm],
        1,
        None,
    )
    .await
    .unwrap();
    assert!(results[0].passes[0].cold_guard.is_none());
}

/// A cold state source that notes the limit it was given, and answers at once.
struct Noting(std::sync::Mutex<Vec<Duration>>);

impl ColdStateSource for Noting {
    fn read(&self) -> Result<Option<crate::result::ColdState>, BenchError> {
        Ok(None)
    }

    fn read_within(&self, limit: Duration) -> crate::coldstate::ReadFuture<'_> {
        self.0.lock().unwrap().push(limit);
        Box::pin(std::future::ready(self.read()))
    }
}

/// Catches a run that reads the cold state with no limit, or with another than the one it was
/// configured with (`--cold-state-timeout-secs`): every pass of every arm asks with it.
#[tokio::test(start_paused = true)]
async fn every_cold_state_reading_is_asked_with_the_configured_limit() {
    let dir = temp("tb-gaps-cold-state-limit");
    let noting = Arc::new(Noting(std::sync::Mutex::default()));
    let mut ctx = testkit::ctx(dir.path(), &[], Window::Quiet);
    ctx.cold_state = noting.clone();
    ctx.config.cold_state_timeout = Duration::from_secs(7);
    let env = Env::new(&ctx, Script::calm()).unwrap();
    run_cell(
        &env,
        CellId::G,
        vec![arm("a", 2, &Arc::new(AtomicU64::new(0)))],
        3,
        None,
    )
    .await
    .unwrap();
    assert_eq!(*noting.0.lock().unwrap(), vec![Duration::from_secs(7); 3]);
}
