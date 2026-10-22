//! Each rule of the estimator has a test here that goes red when the rule is broken;
//! the doc comment of a test names the rule. See the pull request for the planted
//! mutant of each.

use crate::codec::{LEN, checksum};
use crate::*;

const DAY: Day = Day(100);

fn key(basis: Basis, cfg: Option<u64>) -> Key {
    Key::new(
        Instance::new("main"),
        PlatformClass::new("linux-x86_64"),
        basis,
        Shape(7),
        cfg.map(Cfg),
    )
}

fn linux(cfg: u64) -> Key {
    key(Basis::Cgroup, Some(cfg))
}

fn success(peak: u64) -> Run {
    Run {
        outcome: Outcome::Success,
        wall_us: 2_000_000,
        peak_memory_bytes: Some(peak),
        booked_memory_bytes: 4 * GIB,
        limit_bytes: 4 * GIB * 3 / 2 + 512 * MIB,
        raised_memory_bytes: None,
        cpu_usage_us: 0,
        node_cpu_pressure_permille: None,
    }
}

fn kill(booked: u64, used: u64, limit: u64, raised: Option<u64>) -> Run {
    Run {
        outcome: Outcome::OwnLimitKill,
        peak_memory_bytes: Some(used),
        booked_memory_bytes: booked,
        limit_bytes: limit,
        raised_memory_bytes: raised,
        ..success(0)
    }
}

/// A success of 1 GiB that used `par` millicores on average.
fn busy(par: u64, pressure: Option<u16>) -> Run {
    Run {
        cpu_usage_us: par * 2000,
        node_cpu_pressure_permille: pressure,
        ..success(GIB)
    }
}

fn feed(summaries: &mut Summaries, key: &Key, run: &Run, times: usize) {
    for _ in 0..times {
        summaries.observe(key, run, DAY, &Bounds::default());
    }
}

fn estimate(summaries: &Summaries, key: &Key) -> Result<Estimate, Fallback> {
    summaries.estimate(key, DAY, &Bounds::default(), &Hint::default())
}

fn memory(summaries: &Summaries, key: &Key) -> u64 {
    estimate(summaries, key).unwrap().memory_bytes
}

fn floor(summaries: &Summaries, key: &Key) -> u64 {
    summaries.key_floor(key, DAY, &Bounds::default())
}

// ---- 3.2 the memory formula and the statistic -------------------------------------

/// Rule: `E = ceil(1.2 x max + 128 MiB)` rounded up to 256 MiB.
#[test]
fn golden_window_formula() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(3 * GIB), 5);
    // 3 GiB x 1.2 + 128 MiB = 3814.4 MiB -> 3840 MiB.
    assert_eq!(memory(&s, &k), 3840 * MIB);
}

/// Rule: a sum that is already a multiple of 256 MiB stays (no extra step).
#[test]
fn an_exact_multiple_is_not_rounded_further() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(1600 * MIB), 5);
    // 1600 MiB x 1.2 + 128 MiB = 2048 MiB exactly.
    assert_eq!(memory(&s, &k), 2048 * MIB);
}

/// Rule: the statistic is the maximum, so one high sample among 31 low ones sizes the
/// estimate, and the maximum holds for at least 256 runs and then lapses.
#[test]
fn one_high_sample_raises_the_estimate_and_lapses_after_the_span() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(3 * GIB), 1);
    feed(&mut s, &k, &success(GIB), 31);
    assert_eq!(memory(&s, &k), 3840 * MIB);
    feed(&mut s, &k, &success(GIB), 256 - 32);
    assert_eq!(memory(&s, &k), 3840 * MIB, "still in the last 256 runs");
    feed(&mut s, &k, &success(GIB), 288 - 256);
    assert_eq!(
        memory(&s, &k),
        3840 * MIB,
        "the oldest chunk is kept to 288 runs"
    );
    feed(&mut s, &k, &success(GIB), 1);
    assert_eq!(
        memory(&s, &k),
        1536 * MIB,
        "289 runs later the heavy mode is gone"
    );
}

/// Rule: raising is immediate: one sample above the estimate raises it at once.
#[test]
fn a_new_maximum_raises_at_once() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(GIB), 40);
    let before = memory(&s, &k);
    feed(&mut s, &k, &success(6 * GIB), 1);
    assert!(memory(&s, &k) > before);
    // 6 GiB x 1.2 + 128 MiB = 7503.2 MiB -> 7680 MiB.
    assert_eq!(memory(&s, &k), 7680 * MIB);
}

/// Rule: fewer than `N_MIN` samples can only raise: the estimate is at least the
/// default booking; no estimate below two samples without a floor.
#[test]
fn n_below_n_min_never_lowers_below_the_default() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(500 * MIB), 1);
    assert_eq!(estimate(&s, &k), Err(Fallback::Cold));
    for n in 2..N_MIN {
        feed(&mut s, &k, &success(500 * MIB), 1);
        assert_eq!(memory(&s, &k), 4 * GIB, "n = {n}");
    }
    feed(&mut s, &k, &success(500 * MIB), 1);
    assert_eq!(memory(&s, &k), GIB, "n = {N_MIN} may lower to the minimum");
}

/// Rule: with few samples the estimate still raises above the default.
#[test]
fn few_samples_can_raise() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(6 * GIB), 2);
    assert_eq!(memory(&s, &k), 7680 * MIB);
}

// ---- 3.4 OOM feedback -------------------------------------------------------------

/// Rule 3.4.1: a kill sets the key floor to the ladder's next rung, and the next
/// estimate is above the booking it died at.
#[test]
fn a_kill_sets_the_key_floor_above_the_booking() {
    let mut s = Summaries::default();
    let k = linux(1);
    s.observe(
        &k,
        &kill(4 * GIB, 6 * GIB, 6 * GIB + GIB / 2, Some(16 * GIB)),
        DAY,
        &Bounds::default(),
    );
    assert_eq!(floor(&s, &k), 16 * GIB);
    assert_eq!(s.get(&k).unwrap().oom_floor(), 16 * GIB);
    assert!(memory(&s, &k) >= 16 * GIB);
}

/// Rule 3.4.1: with no rung given, the floor is the first 256 MiB step above the
/// booking (not the booking itself), and a rung below the booking cannot lower it.
#[test]
fn a_kill_without_a_rung_floors_one_step_above_the_booking() {
    let mut s = Summaries::default();
    let k = linux(1);
    s.observe(&k, &kill(4 * GIB, GIB, GIB, None), DAY, &Bounds::default());
    assert_eq!(floor(&s, &k), 4 * GIB + 256 * MIB);
    s.observe(
        &k,
        &kill(4 * GIB, GIB, GIB, Some(GIB)),
        DAY,
        &Bounds::default(),
    );
    assert_eq!(floor(&s, &k), 4 * GIB + 256 * MIB);
    assert!(memory(&s, &k) > 4 * GIB);
}

/// A floor above the node cap is held at the cap.
#[test]
fn the_floor_never_exceeds_the_cap() {
    let mut s = Summaries::default();
    let k = linux(1);
    let bounds = Bounds {
        memory_cap: 8 * GIB,
        ..Bounds::default()
    };
    s.observe(
        &k,
        &kill(8 * GIB, 8 * GIB, 8 * GIB, Some(16 * GIB)),
        DAY,
        &bounds,
    );
    assert_eq!(s.key_floor(&k, DAY, &bounds), 8 * GIB);
    let est = s.estimate(&k, DAY, &bounds, &Hint::default()).unwrap();
    assert_eq!(est.memory_bytes, 8 * GIB);
}

/// Rule 3.4.2: the kill enters the maxima as `max(used, limit)`: the estimate rises on
/// it even when the floor is lower.
#[test]
fn a_kill_is_a_censored_sample() {
    let mut s = Summaries::default();
    let k = linux(1);
    s.observe(
        &k,
        &kill(4 * GIB, 5 * GIB, 6 * GIB + GIB / 2, None),
        DAY,
        &Bounds::default(),
    );
    assert_eq!(s.get(&k).unwrap().samples(), 1);
    // 6.5 GiB x 1.2 + 128 MiB = 7.925 GiB -> 8 GiB, above the 4.25 GiB floor.
    assert_eq!(memory(&s, &k), 8 * GIB);
}

/// Rule 3.4.2: when the measured use is above the limit, the use is the sample; when
/// the use is unknown the limit is; neither exceeds the cap.
#[test]
fn the_censored_sample_is_the_larger_of_use_and_limit() {
    let bounds = Bounds::default();
    let sample = |used: Option<u64>, limit: u64| {
        let mut s = Summaries::default();
        let k = linux(1);
        let mut run = kill(4 * GIB, 0, limit, None);
        run.peak_memory_bytes = used;
        s.observe(&k, &run, DAY, &bounds);
        s.get(&k).unwrap().long_max()
    };
    assert_eq!(sample(Some(7 * GIB), 6 * GIB), 7 * GIB);
    assert_eq!(sample(Some(5 * GIB), 6 * GIB), 6 * GIB);
    assert_eq!(sample(None, 6 * GIB), 6 * GIB);
    assert_eq!(sample(Some(64 * GIB), 6 * GIB), 32 * GIB);
}

/// A kill is acted on whatever its wall time (the safety effect does not wait for a
/// "trusted" duration).
#[test]
fn a_short_kill_still_sets_the_floor() {
    let mut s = Summaries::default();
    let k = linux(1);
    let mut run = kill(4 * GIB, 6 * GIB, 6 * GIB, Some(16 * GIB));
    run.wall_us = 0;
    assert_eq!(s.observe(&k, &run, DAY, &Bounds::default()).memory, Ok(()));
    assert_eq!(floor(&s, &k), 16 * GIB);
}

/// Rule 3.4.3: a node-pressure kill never touches the floor or the samples.
#[test]
fn a_node_pressure_kill_is_ignored() {
    let mut s = Summaries::default();
    let k = linux(1);
    let mut run = kill(4 * GIB, 6 * GIB, 6 * GIB, Some(16 * GIB));
    run.outcome = Outcome::NodePressureKill;
    let seen = s.observe(&k, &run, DAY, &Bounds::default());
    assert_eq!(seen.memory, Err(Discard::NotASample));
    assert_eq!(s.get(&k).unwrap().samples(), 0);
    assert_eq!(floor(&s, &k), 0);
}

/// Rule 3.1: a failed (not memory-killed), lost or cancelled run is not a sample.
#[test]
fn a_failed_non_oom_run_is_not_a_sample() {
    for outcome in [Outcome::Failed, Outcome::Lost, Outcome::Cancelled] {
        let mut s = Summaries::default();
        let k = linux(1);
        let run = Run {
            outcome,
            ..success(20 * GIB)
        };
        let seen = s.observe(&k, &run, DAY, &Bounds::default());
        assert_eq!(
            seen,
            Observed {
                memory: Err(Discard::NotASample),
                cpu: false
            }
        );
        assert_eq!(s.get(&k).unwrap().samples(), 0);
        assert_eq!(estimate(&s, &k), Err(Fallback::Cold));
    }
}

/// Rule 3.1: a success is a sample only with a measurement, a wall time of one second,
/// and a peak within the node cap.
#[test]
fn a_success_needs_a_trusted_measurement() {
    let mut s = Summaries::default();
    let k = linux(1);
    let b = Bounds::default();
    let short = Run {
        wall_us: MIN_WALL_US - 1,
        ..success(GIB)
    };
    assert_eq!(
        s.observe(&k, &short, DAY, &b).memory,
        Err(Discard::Untrusted)
    );
    let unmeasured = Run {
        peak_memory_bytes: None,
        ..success(GIB)
    };
    assert_eq!(
        s.observe(&k, &unmeasured, DAY, &b).memory,
        Err(Discard::Untrusted)
    );
    let huge = success(b.memory_cap + 1);
    assert_eq!(s.observe(&k, &huge, DAY, &b).memory, Err(Discard::OverCap));
    assert_eq!(s.get(&k).unwrap().samples(), 0);
    let exact = Run {
        wall_us: MIN_WALL_US,
        ..success(b.memory_cap)
    };
    assert_eq!(s.observe(&k, &exact, DAY, &b).memory, Ok(()));
    assert_eq!(s.get(&k).unwrap().samples(), 1);
}

/// A kill sample is a sample even though its peak is unmeasured and its wall is short.
fn heavy_kill() -> Run {
    kill(
        4 * GIB,
        6 * GIB + GIB / 2,
        6 * GIB + GIB / 2,
        Some(16 * GIB),
    )
}

/// Rule 3.4.4: a heavy mode that recurs every 30 runs never lets the floor go.
#[test]
fn a_recurring_heavy_mode_never_decays_the_floor() {
    let mut s = Summaries::default();
    let k = linux(1);
    s.observe(&k, &heavy_kill(), DAY, &Bounds::default());
    for i in 1..=3000 {
        let peak = if i % 30 == 0 { 9 * GIB } else { 500 * MIB };
        feed(&mut s, &k, &success(peak), 1);
        assert_eq!(floor(&s, &k), 16 * GIB, "run {i}");
    }
}

/// Rule 3.4.4: the floor halves after 256 runs without the heavy mode, once per 256
/// runs, down to nothing below the minimum estimate.
#[test]
fn the_floor_halves_after_256_runs_without_the_heavy_mode() {
    let mut s = Summaries::default();
    let k = linux(1);
    s.observe(&k, &heavy_kill(), DAY, &Bounds::default());
    feed(&mut s, &k, &success(500 * MIB), 255);
    assert_eq!(floor(&s, &k), 16 * GIB, "255 runs are not enough");
    feed(&mut s, &k, &success(500 * MIB), 1);
    assert_eq!(floor(&s, &k), 8 * GIB, "the 256th run halves it");
    // The kill itself (6.5 GiB, doubled 13 GiB) is above 8 GiB until its chunk lapses.
    feed(&mut s, &k, &success(500 * MIB), 255);
    assert_eq!(floor(&s, &k), 8 * GIB);
    for (runs, expect) in [(1, 4), (256, 2), (256, 1)] {
        feed(&mut s, &k, &success(500 * MIB), runs);
        assert_eq!(floor(&s, &k), expect * GIB, "after {runs} more");
    }
    feed(&mut s, &k, &success(500 * MIB), 255);
    assert_eq!(floor(&s, &k), GIB);
    feed(&mut s, &k, &success(500 * MIB), 1);
    assert_eq!(floor(&s, &k), 0, "half of 1 GiB is under the minimum");
}

/// Rule 3.4.5: two kills within 256 runs make the key raise-only for 7 days: it keeps
/// its floor, never lowers below the default, and does not fall back to "no estimate".
#[test]
fn two_kills_in_256_runs_make_the_key_raise_only() {
    let mut s = Summaries::default();
    let k = linux(1);
    let b = Bounds::default();
    s.observe(
        &k,
        &kill(GIB + GIB / 2, 2 * GIB, 2 * GIB, Some(2 * GIB)),
        DAY,
        &b,
    );
    feed(&mut s, &k, &success(500 * MIB), 100);
    assert_eq!(
        memory(&s, &k),
        2816 * MIB,
        "one kill: lowered below the default"
    );
    s.observe(&k, &kill(2 * GIB, 3 * GIB, 3 * GIB, Some(3 * GIB)), DAY, &b);
    assert_eq!(memory(&s, &k), 4 * GIB, "raise-only: not below the default");
    assert!(s.get(&k).unwrap().oom_floor() >= 3 * GIB);
    let at = |day| s.estimate(&k, day, &b, &Hint::default());
    assert_eq!(at(Day(DAY.0 + 6)).unwrap().memory_bytes, 4 * GIB);
    assert_eq!(
        at(Day(DAY.0 + 7)).unwrap().memory_bytes,
        3840 * MIB,
        "the seventh day lowers again"
    );
}

/// Rule 3.4.5: while raise-only the floor does not decay, however quiet; it decays on
/// the first run after the state ends.
#[test]
fn a_raise_only_key_keeps_its_floor() {
    let mut s = Summaries::default();
    let k = linux(1);
    let b = Bounds::default();
    s.observe(&k, &kill(4 * GIB, 6 * GIB, 6 * GIB, Some(8 * GIB)), DAY, &b);
    s.observe(
        &k,
        &kill(8 * GIB, 12 * GIB, 12 * GIB, Some(16 * GIB)),
        DAY,
        &b,
    );
    feed(&mut s, &k, &success(500 * MIB), 600);
    assert_eq!(floor(&s, &k), 16 * GIB);
    let later = Day(DAY.0 + 7);
    s.observe(&k, &success(500 * MIB), later, &b);
    assert_eq!(s.key_floor(&k, later, &b), 8 * GIB);
}

/// Rule 3.4.5 boundary: the second kill must be within 256 runs of the first.
#[test]
fn the_second_kill_must_be_within_256_runs() {
    let b = Bounds::default();
    for (between, raise_only) in [(254, true), (255, false)] {
        let mut s = Summaries::default();
        let k = linux(1);
        s.observe(&k, &heavy_kill(), DAY, &b);
        feed(&mut s, &k, &success(500 * MIB), between);
        s.observe(&k, &heavy_kill(), DAY, &b);
        assert_eq!(
            s.get(&k).unwrap().raise_only(DAY),
            raise_only,
            "{between} between"
        );
    }
}

// ---- 2.1 the key -------------------------------------------------------------------

/// Rule 2.1: Mac and Linux samples never mix.
#[test]
fn bases_never_mix() {
    let mut s = Summaries::default();
    let mac = key(Basis::MacFootprint, Some(1));
    let lin = key(Basis::Cgroup, Some(1));
    feed(&mut s, &mac, &success(3 * GIB), 5);
    s.observe(&mac, &heavy_kill(), DAY, &Bounds::default());
    assert_eq!(estimate(&s, &lin), Err(Fallback::Missing));
    assert_eq!(floor(&s, &lin), 0);
    assert!(memory(&s, &mac) >= 16 * GIB);
    feed(&mut s, &lin, &success(GIB), 5);
    assert_eq!(memory(&s, &lin), 1536 * MIB);
    assert_eq!(s.get(&mac).unwrap().basis(), Basis::MacFootprint);
}

/// Rule 2.3: the full key (L1) answers once it has `N_MIN` samples, otherwise the
/// shape-level summary (L2), which merges configurations.
#[test]
fn the_levels_answer_in_order() {
    let mut s = Summaries::default();
    feed(&mut s, &linux(1), &success(3 * GIB), 10);
    // A new configuration has one sample: L1 cannot answer, L2 (the shape) does.
    feed(&mut s, &linux(2), &success(GIB), 1);
    let est = estimate(&s, &linux(2)).unwrap();
    assert_eq!((est.level, est.memory_bytes), (Level::L2, 3840 * MIB));
    // Exactly N_MIN of its own: L1 answers with its own, smaller, maximum.
    feed(&mut s, &linux(2), &success(GIB), 4);
    let est = estimate(&s, &linux(2)).unwrap();
    assert_eq!((est.level, est.memory_bytes), (Level::L1, 1536 * MIB));
    // A key with no configuration has only L2.
    let none = key(Basis::Cgroup, None);
    let est = estimate(&s, &none).unwrap();
    assert_eq!((est.level, est.memory_bytes), (Level::L2, 3840 * MIB));
}

/// A kill under one configuration floors the whole shape: another configuration is
/// floored too (L2 merges, over-books, never under-books).
#[test]
fn a_shape_floor_applies_to_every_configuration() {
    let mut s = Summaries::default();
    s.observe(&linux(1), &heavy_kill(), DAY, &Bounds::default());
    feed(&mut s, &linux(2), &success(GIB), 5);
    assert_eq!(estimate(&s, &linux(2)).unwrap().level, Level::L1);
    assert!(memory(&s, &linux(2)) >= 16 * GIB);
    assert_eq!(floor(&s, &linux(2)), 16 * GIB);
}

// ---- 2.4 hints ----------------------------------------------------------------------

/// Rule 2.4: the client's hint is never lowered, for memory and CPU; a hint above the
/// cap is passed through (the scheduler's existing path rejects it).
#[test]
fn a_hint_is_never_lowered() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(500 * MIB), 5);
    let ask = |memory_bytes, cpu_millis| {
        s.estimate(
            &k,
            DAY,
            &Bounds::default(),
            &Hint {
                memory_bytes,
                cpu_millis,
            },
        )
        .unwrap()
    };
    let est = ask(Some(12 * GIB), Some(4000));
    assert_eq!((est.memory_bytes, est.cpu_millis), (12 * GIB, 4000));
    let est = ask(Some(512 * MIB), Some(500));
    assert_eq!((est.memory_bytes, est.cpu_millis), (GIB, 1000));
    assert_eq!(ask(Some(40 * GIB), None).memory_bytes, 40 * GIB);
    assert_eq!(ask(None, None).memory_bytes, GIB);
}

// ---- 3.3 CPU -------------------------------------------------------------------------

fn cpu_of(runs: &[(u64, Option<u16>, usize)], bounds: &Bounds) -> u32 {
    let mut s = Summaries::default();
    let k = linux(1);
    for (par, pressure, times) in runs {
        for _ in 0..*times {
            s.observe(&k, &busy(*par, *pressure), DAY, bounds);
        }
    }
    s.estimate(&k, DAY, bounds, &Hint::default())
        .unwrap()
        .cpu_millis
}

/// Rule 3.3: samples taken under node CPU pressure (or with no pressure reading) are
/// dropped; the threshold is exclusive.
#[test]
fn cpu_samples_under_node_pressure_are_dropped() {
    let b = Bounds::default();
    assert_eq!(cpu_of(&[(4000, Some(500), 32)], &b), 1000);
    assert_eq!(cpu_of(&[(4000, None, 32)], &b), 1000);
    assert_eq!(
        cpu_of(&[(4000, Some(CPU_PRESSURE_MAX_PERMILLE), 32)], &b),
        1000
    );
    assert_eq!(
        cpu_of(&[(4000, Some(CPU_PRESSURE_MAX_PERMILLE - 1), 32)], &b),
        4000
    );
    let mut s = Summaries::default();
    let seen = s.observe(&linux(1), &busy(4000, Some(500)), DAY, &b);
    assert!(!seen.cpu && seen.memory.is_ok());
    let failed = Run {
        outcome: Outcome::Failed,
        ..busy(4000, Some(0))
    };
    assert!(!s.observe(&linux(1), &failed, DAY, &b).cpu);
    let short = Run {
        wall_us: 10,
        ..busy(4000, Some(0))
    };
    assert!(!s.observe(&linux(1), &short, DAY, &b).cpu);
}

/// Rule 3.3: the estimate is the nearest-rank P90 of the last 32 samples.
#[test]
fn cpu_is_the_p90_of_the_recent_samples() {
    let b = Bounds::default();
    assert_eq!(cpu_of(&[(1500, Some(0), 28), (4000, Some(0), 4)], &b), 4000);
    assert_eq!(cpu_of(&[(1500, Some(0), 29), (4000, Some(0), 3)], &b), 1500);
    // Only the last 32 count: 32 heavy samples are replaced by 32 light ones.
    assert_eq!(
        cpu_of(&[(4000, Some(0), 32), (1000, Some(0), 32)], &b),
        1000
    );
}

/// Rule 3.3: rounded up to 250 m, never below 1000 m, never above the node.
#[test]
fn cpu_is_rounded_up_and_clamped() {
    let b = Bounds::default();
    assert_eq!(cpu_of(&[(1501, Some(0), 10)], &b), 1750);
    assert_eq!(cpu_of(&[(1750, Some(0), 10)], &b), 1750);
    assert_eq!(cpu_of(&[(200, Some(0), 10)], &b), CPU_MIN_MILLIS);
    let small = Bounds {
        node_cpu_millis: 16_000,
        ..b
    };
    assert_eq!(cpu_of(&[(64_000, Some(0), 10)], &small), 16_000);
    let tiny = Bounds {
        node_cpu_millis: 500,
        ..b
    };
    assert_eq!(cpu_of(&[(64_000, Some(0), 10)], &tiny), 500);
}

// ---- 3.5 storage ---------------------------------------------------------------------

fn lived_in() -> Summary {
    let mut s = Summaries::default();
    let k = linux(1);
    let mut rng = Rng(7);
    for _ in 0..700 {
        let run = if rng.below(50) == 0 {
            heavy_kill()
        } else {
            busy(rng.below(6000), Some(0))
        };
        s.observe(&k, &run, DAY, &Bounds::default());
    }
    s.get(&k).unwrap().clone()
}

/// Rule 3.5: a summary round-trips, and the bytes are deterministic and fixed-size.
#[test]
fn a_summary_round_trips() {
    let summary = lived_in();
    assert!(summary.oom_floor() > 0 && summary.samples() == 700);
    let bytes = summary.encode();
    assert_eq!(bytes.len(), LEN);
    assert_eq!(bytes, summary.encode());
    assert_eq!(Summary::decode(&bytes), Some(summary));
    let fresh = Summary::new(Basis::MacFootprint, Day(3));
    assert_eq!(Summary::decode(&fresh.encode()), Some(fresh));
}

fn reseal(mut bytes: Vec<u8>) -> Vec<u8> {
    let sum = checksum(&bytes[..LEN - 8]);
    bytes[LEN - 8..].copy_from_slice(&sum.to_le_bytes());
    bytes
}

/// Rule 3.5: anything that is not exactly one valid record decodes to `None`.
#[test]
fn a_corrupt_row_does_not_decode() {
    let bytes = lived_in().encode();
    assert_eq!(Summary::decode(&[]), None);
    assert_eq!(Summary::decode(&bytes[..LEN - 1]), None);
    assert_eq!(Summary::decode(&[bytes.clone(), vec![0]].concat()), None);
    for at in 0..LEN {
        let mut flipped = bytes.clone();
        flipped[at] ^= 0x01;
        assert_eq!(Summary::decode(&flipped), None, "byte {at}");
    }
    // Damage that keeps the checksum valid is caught by the field checks.
    let with = |at: usize, value: &[u8]| {
        let mut bytes = bytes.clone();
        bytes[at..at + value.len()].copy_from_slice(value);
        reseal(bytes)
    };
    assert_eq!(Summary::decode(&with(0, &[2])), None, "version");
    assert_eq!(Summary::decode(&with(1, &[3])), None, "basis");
    assert_eq!(Summary::decode(&with(1, &[0])), None, "basis 0");
    let n = u64::from_le_bytes(bytes[2..10].try_into().unwrap());
    let beyond = (n + 1).to_le_bytes();
    assert!(
        Summary::decode(&with(2, &0u64.to_le_bytes())).is_none(),
        "anchor and kill after n"
    );
    assert!(
        Summary::decode(&with(30, &beyond)).is_none(),
        "anchor after n"
    );
    assert!(
        Summary::decode(&with(38, &beyond)).is_none(),
        "kill after n"
    );
}

/// Rule 3.5: a stored row that does not decode, or is another basis, leaves the key
/// cold; a good row restores the same estimate.
#[test]
fn a_corrupt_row_leaves_the_key_cold() {
    let mut original = Summaries::default();
    let k = linux(1);
    original.observe(&k, &heavy_kill(), DAY, &Bounds::default());
    feed(&mut original, &k, &success(GIB), 10);
    let bytes = original.get(&k.l2()).unwrap().encode();

    let mut restored = Summaries::default();
    let mut bad = bytes.clone();
    bad[40] ^= 0xff;
    assert_eq!(
        restored.insert_encoded(&k.l2(), &bad),
        Err(Fallback::Corrupt)
    );
    assert!(restored.is_empty());
    assert_eq!(estimate(&restored, &k.l2()), Err(Fallback::Missing));
    let mac = key(Basis::MacFootprint, None);
    assert_eq!(
        restored.insert_encoded(&mac, &bytes),
        Err(Fallback::Corrupt)
    );
    assert_eq!(restored.insert_encoded(&k.l2(), &bytes), Ok(()));
    assert_eq!(restored.len(), 1);
    assert_eq!(floor(&restored, &k), 16 * GIB);
    assert_eq!(estimate(&restored, &k.l2()), estimate(&original, &k.l2()));
}

// ---- 3.5 / 4.3 staleness and bounds on the map ------------------------------------------

/// Rule 4.3: idle more than 60 days is stale (no estimate, no floor); expire drops it.
#[test]
fn a_stale_summary_is_no_estimate() {
    let mut s = Summaries::default();
    let k = linux(1);
    let b = Bounds::default();
    assert_eq!(estimate(&s, &k), Err(Fallback::Missing));
    feed(&mut s, &k, &success(GIB), 5);
    s.observe(&k, &heavy_kill(), DAY, &b);
    let at = |days: u32| s.estimate(&k, Day(DAY.0 + days), &b, &Hint::default());
    assert!(at(STALE_DAYS).is_ok());
    assert_eq!(at(STALE_DAYS + 1), Err(Fallback::Stale));
    assert_eq!(s.key_floor(&k, Day(DAY.0 + STALE_DAYS), &b), 16 * GIB);
    assert_eq!(s.key_floor(&k, Day(DAY.0 + STALE_DAYS + 1), &b), 0);
    // A clock that reads before the last sighting is not stale.
    assert!(s.estimate(&k, Day(DAY.0 - 5), &b, &Hint::default()).is_ok());
    s.expire(Day(DAY.0 + STALE_DAYS));
    assert_eq!(s.len(), 2);
    s.expire(Day(DAY.0 + STALE_DAYS + 1));
    assert!(s.is_empty());
}

/// Rule 3.2: at most `max_keys` keys; the least recently seen go, ties by key, and the
/// key just observed is never the one dropped.
#[test]
fn the_oldest_keys_are_evicted() {
    let b = Bounds::default();
    let mut s = Summaries::with_max_keys(4);
    let none = |shape: u64| {
        Key::new(
            Instance::new("main"),
            PlatformClass::new("p"),
            Basis::Cgroup,
            Shape(shape),
            None,
        )
    };
    for (shape, day) in [(5, 10), (6, 11), (7, 12), (8, 13)] {
        s.observe(&none(shape), &success(GIB), Day(day), &b);
    }
    assert_eq!(s.len(), 4);
    s.observe(&none(9), &success(GIB), Day(14), &b);
    assert_eq!(s.len(), 4);
    assert!(s.get(&none(5)).is_none() && s.get(&none(9)).is_some());
    // A new key that is older than every other is still kept; the oldest other goes.
    s.observe(&none(1), &success(GIB), Day(1), &b);
    assert!(s.get(&none(1)).is_some() && s.get(&none(6)).is_none());
    // Equal days: the smaller key goes.
    let mut tie = Summaries::with_max_keys(2);
    for shape in [3, 2, 4] {
        tie.observe(&none(shape), &success(GIB), Day(1), &b);
    }
    assert!(
        tie.get(&none(2)).is_none() && tie.get(&none(3)).is_some() && tie.get(&none(4)).is_some()
    );
    // A key with a configuration needs two slots; a limit under two is raised to two.
    let mut two = Summaries::with_max_keys(0);
    two.observe(&linux(1), &success(GIB), DAY, &b);
    assert_eq!(two.len(), 2);
    two.observe(&linux(2), &success(GIB), Day(DAY.0 + 1), &b);
    assert_eq!(two.len(), 2);
    assert!(two.get(&linux(2)).is_some() && two.get(&linux(2).l2()).is_some());
}

// ---- 4.1 properties ----------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// Mostly a plausible size, sometimes an extreme.
    fn size(&mut self, scale: u64) -> u64 {
        match self.below(20) {
            0 => u64::MAX,
            1 => 0,
            _ => self.below(scale.saturating_mul(2).max(1)),
        }
    }
}

fn random_run(rng: &mut Rng, cap: u64) -> Run {
    let outcome = [
        Outcome::Success,
        Outcome::Success,
        Outcome::Success,
        Outcome::OwnLimitKill,
        Outcome::NodePressureKill,
        Outcome::Failed,
        Outcome::Lost,
        Outcome::Cancelled,
    ][rng.below(8) as usize];
    Run {
        outcome,
        wall_us: if rng.below(4) == 0 {
            rng.size(3_000_000)
        } else {
            2_000_000
        },
        peak_memory_bytes: (rng.below(10) != 0).then(|| rng.size(cap)),
        booked_memory_bytes: rng.size(cap),
        limit_bytes: rng.size(cap),
        raised_memory_bytes: (rng.below(3) != 0).then(|| rng.size(cap)),
        cpu_usage_us: rng.size(40_000_000),
        node_cpu_pressure_permille: (rng.below(5) != 0).then(|| rng.below(300) as u16),
    }
}

/// Rules 4.1 and 3.2: on arbitrary input (extremes included) nothing panics, the
/// estimate stays inside its bounds, never falls under the key floor, is at least the
/// default until `N_MIN` samples, and after a kill is above the booking it died at (or
/// at the cap).
#[test]
fn estimates_are_total_bounded_and_above_the_floor() {
    let caps = [512 * MIB, 2 * GIB, 8 * GIB, 32 * GIB, 1 << 50];
    for seed in 1..=60u64 {
        let mut rng = Rng(seed * 0x9e37_79b9);
        let cap = caps[rng.below(caps.len() as u64) as usize];
        let bounds = Bounds {
            default_memory: rng.below(8) * GIB,
            memory_cap: cap,
            node_cpu_millis: rng.below(70_000) as u32,
        };
        let keys = [linux(1), linux(2), key(Basis::Cgroup, None)];
        let mut s = Summaries::with_max_keys(5);
        let mut day = Day(10);
        for _ in 0..250 {
            day = Day(day.0 + rng.below(3) as u32);
            let k = &keys[rng.below(3) as usize];
            let run = random_run(&mut rng, cap);
            s.observe(k, &run, day, &bounds);
            for probe in &keys {
                let Ok(est) = s.estimate(probe, day, &bounds, &Hint::default()) else {
                    continue;
                };
                let low = MIN_MEMORY.min(cap);
                assert!((low..=cap).contains(&est.memory_bytes), "{est:?} cap {cap}");
                let cpu_low = CPU_MIN_MILLIS.min(bounds.node_cpu_millis);
                assert!((cpu_low..=bounds.node_cpu_millis).contains(&est.cpu_millis));
                assert!(est.memory_bytes >= s.key_floor(probe, day, &bounds));
                let own = s.get(probe).map_or(0, Summary::samples);
                if own < N_MIN && probe.l2() == *probe {
                    assert!(est.memory_bytes >= bounds.default_memory.min(cap));
                }
            }
            if run.outcome == Outcome::OwnLimitKill {
                let est = s.estimate(k, day, &bounds, &Hint::default()).unwrap();
                let booked = run.booked_memory_bytes;
                assert!(
                    est.memory_bytes > booked || est.memory_bytes == cap,
                    "{est:?} {run:?}"
                );
            }
        }
    }
}

/// Totality at the extremes: every field at its maximum or minimum.
#[test]
fn extreme_inputs_do_not_panic() {
    let bounds = Bounds {
        default_memory: u64::MAX,
        memory_cap: u64::MAX,
        node_cpu_millis: u32::MAX,
    };
    let mut s = Summaries::default();
    let k = linux(1);
    for outcome in [Outcome::Success, Outcome::OwnLimitKill] {
        let run = Run {
            outcome,
            wall_us: u64::MAX,
            peak_memory_bytes: Some(u64::MAX),
            booked_memory_bytes: u64::MAX,
            limit_bytes: u64::MAX,
            raised_memory_bytes: Some(u64::MAX),
            cpu_usage_us: u64::MAX,
            node_cpu_pressure_permille: Some(0),
        };
        for _ in 0..10 {
            s.observe(&k, &run, Day(u32::MAX), &bounds);
        }
    }
    let hint = Hint {
        memory_bytes: Some(u64::MAX),
        cpu_millis: Some(u32::MAX),
    };
    let est = s.estimate(&k, Day(u32::MAX), &bounds, &hint).unwrap();
    assert_eq!((est.memory_bytes, est.cpu_millis), (u64::MAX, u32::MAX));
}
