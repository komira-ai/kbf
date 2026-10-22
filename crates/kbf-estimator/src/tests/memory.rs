//! The memory estimate: formula, statistic, samples, censoring, hierarchy, hosts, hints, CPU.

use super::*;

// ---- the formula and the statistic ------------------------------------------------

/// Rule: `E = ceil(1.2 x max + 128 MiB)` rounded up to 256 MiB, in integers.
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
    assert_eq!(memory(&s, &k), 1536 * MIB);
    feed(&mut s, &k, &success(3 * GIB), 1);
    assert_eq!(memory(&s, &k), 3840 * MIB);
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
    // 6 GiB x 1.2 + 128 MiB = 7503.2 MiB -> 7680 MiB.
    assert_eq!(memory(&s, &k), 7680 * MIB);
}

// ---- samples: which runs count -----------------------------------------------------

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

/// Rule 3.1: a success is a sample only with a non-zero measurement, a wall time of
/// one second, and a peak under the host's memory.
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
    let zero = Run {
        peak_memory_bytes: Some(0),
        ..success(GIB)
    };
    assert_eq!(
        s.observe(&k, &zero, DAY, &b).memory,
        Err(Discard::Untrusted)
    );
    let whole_host = success(b.host_memory);
    assert_eq!(
        s.observe(&k, &whole_host, DAY, &b).memory,
        Err(Discard::OverCap)
    );
    assert_eq!(s.get(&k).unwrap().samples(), 0);
    let exact = Run {
        wall_us: MIN_WALL_US,
        ..success(b.host_memory - 1)
    };
    assert_eq!(s.observe(&k, &exact, DAY, &b).memory, Ok(()));
    assert_eq!(s.get(&k).unwrap().samples(), 1);
}

/// M7: zero is not absent. A run with a zero or missing field never changes the
/// estimate or the stored summary, over arbitrary streams.
#[test]
fn a_zero_field_never_changes_the_estimate() {
    let b = Bounds::default();
    for seed in 1..=20u64 {
        let mut rng = Rng(seed * 0x2545_f491);
        let k = linux(1);
        let (mut plain, mut noisy) = (Summaries::default(), Summaries::default());
        for _ in 0..120 {
            let run = Run {
                cpu_usage_us: rng.below(4_000_000) + 1,
                node_cpu_pressure_permille: Some(0),
                ..success(rng.below(3 * GIB) + 1)
            };
            plain.observe(&k, &run, DAY, &b);
            noisy.observe(&k, &run, DAY, &b);
            let zeros = [
                Run {
                    peak_memory_bytes: Some(0),
                    cpu_usage_us: 0,
                    ..run
                },
                Run {
                    peak_memory_bytes: None,
                    cpu_usage_us: 0,
                    ..run
                },
                Run { wall_us: 0, ..run },
            ];
            for zero in zeros {
                let seen = noisy.observe(&k, &zero, DAY, &b);
                assert!(seen.memory.is_err() && !seen.cpu, "{zero:?}");
            }
            assert_eq!(plain.get(&k), noisy.get(&k));
            assert_eq!(
                plain.estimate(&k, DAY, &b, &Hint::default()),
                noisy.estimate(&k, DAY, &b, &Hint::default())
            );
        }
    }
    // A zero CPU time alone drops the CPU sample but keeps the memory sample.
    let mut s = Summaries::default();
    let idle = Run {
        node_cpu_pressure_permille: Some(0),
        ..success(GIB)
    };
    let seen = s.observe(&linux(1), &idle, DAY, &b);
    assert!(seen.memory.is_ok() && !seen.cpu);
}

// ---- M2: a run that hit its limit is censored ----------------------------------------

fn held_estimate(run: Run, times: usize) -> u64 {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &run, times);
    memory(&s, &k)
}

/// M2: a success that reached its limit (any of: the limit was hit, it swapped, its
/// peak is within 5% of the limit) contributes `min(peak, booked)`: it holds the
/// estimate at the booking and gives it no headroom.
#[test]
fn a_success_that_hit_its_limit_holds_the_estimate() {
    let at_limit = Run {
        booked_memory_bytes: 4 * GIB,
        ..success(6 * GIB + GIB / 2)
    };
    assert_eq!(held_estimate(at_limit, 5), 4 * GIB, "near the limit");
    let hits = Run {
        limit_hits: 1,
        booked_memory_bytes: 4 * GIB,
        ..success(GIB)
    };
    assert_eq!(
        held_estimate(hits, 5),
        GIB,
        "memory.events max, sample min(peak, booked)"
    );
    let hits_high = Run {
        limit_hits: 3,
        booked_memory_bytes: 4 * GIB,
        ..success(5 * GIB)
    };
    assert_eq!(held_estimate(hits_high, 5), 4 * GIB, "min(peak, booked)");
    let swapped = Run {
        swap_peak_bytes: 1,
        booked_memory_bytes: 4 * GIB,
        ..success(3 * GIB)
    };
    assert_eq!(held_estimate(swapped, 5), 3 * GIB, "swap");
    let plain = Run {
        booked_memory_bytes: 4 * GIB,
        ..success(3 * GIB)
    };
    assert_eq!(
        held_estimate(plain, 5),
        3840 * MIB,
        "no sign of the limit: headroom applies"
    );
}

/// M2: the 95% boundary is inclusive and exact in integers.
#[test]
fn the_near_limit_boundary_is_exact() {
    let booked = 8 * GIB;
    let limit = memory_max(booked).unwrap();
    assert_eq!(limit % 20, 0);
    let edge = limit / 100 * 95;
    let run = |peak| Run {
        booked_memory_bytes: booked,
        ..success(peak)
    };
    assert_eq!(
        held_estimate(run(edge), 5),
        8 * GIB,
        "at 95%: censored, held at booked"
    );
    assert_eq!(
        held_estimate(run(edge - 1), 5),
        14 * GIB + GIB / 2,
        "just under: uncensored"
    );
}

/// M2: censored successes never raise an estimate past the booking; only a kill does.
#[test]
fn a_censored_success_never_raises_past_the_booking() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(GIB), 10);
    let saturated = Run {
        booked_memory_bytes: 4 * GIB,
        ..success(6 * GIB + GIB / 2)
    };
    feed(&mut s, &k, &saturated, 40);
    assert_eq!(memory(&s, &k), 4 * GIB);
    s.observe(&k, &kill(4 * GIB, 0, None), DAY, &Bounds::default());
    assert!(memory(&s, &k) > 4 * GIB);
}

/// M2 closed loop: peak(B) = min(anon + fill, limit(B)), 200 runs per key, the booking
/// follows the estimate. Where the key fits it settles within 1.2 x (anon + fill) plus
/// the slack and a step; where the first limit cuts it off (anon + fill at least 95% of
/// the 6.5 GiB limit of a 4 GiB booking) the booking stays at 4 GiB, because a run
/// that only reached its limit never raises it.
#[test]
fn a_closed_loop_settles_and_never_climbs_on_a_saturated_key() {
    let b = Bounds::default();
    for (seed, need) in [
        (1, 2 * GIB),
        (2, 4 * GIB),
        (3, 5 * GIB),
        (4, 6 * GIB),
        (5, 8 * GIB),
        (6, 10 * GIB),
        (7, 24 * GIB),
    ] {
        let mut rng = Rng(seed * 0x9e37_79b9);
        let (mut s, k) = (Summaries::default(), linux(1));
        let mut top = 0;
        for _ in 0..200 {
            let booked = s
                .estimate(&k, DAY, &b, &Hint::default())
                .map_or(b.default_memory, |est| est.memory_bytes);
            top = top.max(booked);
            let wanted = need - rng.below(need / 5);
            let limit = memory_max(booked).unwrap();
            let run = Run {
                booked_memory_bytes: booked,
                ..success(wanted.min(limit))
            };
            s.observe(&k, &run, DAY, &b);
        }
        let ceiling = (need + need.div_ceil(5) + 128 * MIB).div_ceil(256 * MIB) * 256 * MIB;
        if need * 100 >= memory_max(4 * GIB).unwrap() * 95 {
            assert_eq!(top, 4 * GIB, "need {need}: a saturated key must not climb");
        } else {
            assert!(
                top <= ceiling.max(4 * GIB),
                "need {need}: top {top} > {ceiling}"
            );
        }
    }
}

/// S5: a kill is a sample of at least the limit derived from its booking, not of the
/// record's `used` (0 when the killed run carried no usage).
#[test]
fn a_kill_sample_uses_the_derived_limit() {
    let mut s = Summaries::default();
    let k = linux(1);
    s.observe(&k, &kill(4 * GIB, 0, None), DAY, &Bounds::default());
    assert_eq!(s.get(&k).unwrap().long_max(), 6 * GIB + GIB / 2);
    assert_eq!(memory_max(0), None);
    assert_eq!(memory_max(4 * GIB), Some(6 * GIB + GIB / 2));
}

/// A kill with nothing booked and nothing measured has no sample to give and no
/// booking to raise from: it is not acted on.
#[test]
fn a_kill_with_nothing_booked_or_measured_is_ignored() {
    let mut s = Summaries::default();
    let k = linux(1);
    let nothing = Run {
        peak_memory_bytes: None,
        ..kill(0, 0, None)
    };
    let seen = s.observe(&k, &nothing, DAY, &Bounds::default());
    assert_eq!(seen.memory, Err(Discard::Untrusted));
    assert_eq!(s.get(&k).unwrap().samples(), 0);
    assert_eq!(floor(&s, &k), 0);
    assert_eq!(estimate(&s, &k), Err(Fallback::Cold));
}

/// Rule 3.4.2: a kill enters the maxima as a sample (with headroom), so the estimate
/// rises on it even when the floor is lower.
#[test]
fn a_kill_is_a_censored_sample() {
    let mut s = Summaries::default();
    let k = linux(1);
    s.observe(&k, &kill(4 * GIB, 5 * GIB, None), DAY, &Bounds::default());
    assert_eq!(s.get(&k).unwrap().samples(), 1);
    // 6.5 GiB x 1.2 + 128 MiB = 7.925 GiB -> 8 GiB, above the 4.25 GiB floor.
    assert_eq!(memory(&s, &k), 8 * GIB);
}

/// Rule 3.4.2: when the measured use is above the derived limit, the use is the
/// sample; neither exceeds the host.
#[test]
fn the_kill_sample_is_the_larger_of_use_and_limit() {
    let bounds = Bounds::default();
    let sample = |used: Option<u64>, booked: u64| {
        let mut s = Summaries::default();
        let k = linux(1);
        let mut run = kill(booked, 0, None);
        run.peak_memory_bytes = used;
        s.observe(&k, &run, DAY, &bounds);
        s.get(&k).unwrap().long_max()
    };
    assert_eq!(sample(Some(7 * GIB), 4 * GIB), 7 * GIB);
    assert_eq!(sample(Some(5 * GIB), 4 * GIB), 6 * GIB + GIB / 2);
    assert_eq!(sample(None, 4 * GIB), 6 * GIB + GIB / 2);
    assert_eq!(sample(Some(100 * GIB), 4 * GIB), 64 * GIB - 1);
}

// ---- S4: mixed keys -------------------------------------------------------------------

/// S4: a key whose recent samples are far apart is mixed: never lowered, and raised
/// only by a kill.
#[test]
fn a_mixed_key_is_never_lowered_and_raised_only_by_kills() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(500 * MIB), 10);
    feed(&mut s, &k, &success(5 * GIB / 2), 1);
    assert!(s.get(&k).unwrap().mixed());
    assert_eq!(
        memory(&s, &k),
        4 * GIB,
        "mixed: the default, not the sized 3.25 GiB"
    );
    feed(&mut s, &k, &success(5 * GIB), 1);
    assert_eq!(memory(&s, &k), 4 * GIB, "samples do not raise a mixed key");
    s.observe(&k, &heavy_kill(), DAY, &Bounds::default());
    assert!(memory(&s, &k) >= 16 * GIB, "a kill does");
}

/// S4: the dispersion test is strict: exactly four times the median is not mixed.
#[test]
fn the_mixed_boundary_is_exact() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(GIB), 20);
    feed(&mut s, &k, &success(4 * GIB), 1);
    assert!(!s.get(&k).unwrap().mixed());
    assert_eq!(memory(&s, &k), 5 * GIB);
    feed(&mut s, &k, &success(4 * GIB + 1), 1);
    assert!(s.get(&k).unwrap().mixed());
    assert_eq!(memory(&s, &k), 4 * GIB);
    assert!(
        !Summary::new(Basis::Cgroup, DAY).mixed(),
        "no samples is not mixed"
    );
}

// ---- 2.1 / 2.3 the key and the hierarchy ----------------------------------------------

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

/// Rule 2.1: instance and platform class separate keys.
#[test]
fn instances_and_platforms_never_mix() {
    let mut s = Summaries::default();
    let k = linux(1);
    feed(&mut s, &k, &success(3 * GIB), 5);
    s.observe(&k, &heavy_kill(), DAY, &Bounds::default());
    let other_instance = Key::new(
        Instance::new("other"),
        PlatformClass::new("linux-x86_64"),
        Basis::Cgroup,
        Shape(7),
        Some(Cfg(1)),
    );
    let other_platform = Key::new(
        Instance::new("main"),
        PlatformClass::new("linux-arm64"),
        Basis::Cgroup,
        Shape(7),
        Some(Cfg(1)),
    );
    let other_shape = Key::new(
        Instance::new("main"),
        PlatformClass::new("linux-x86_64"),
        Basis::Cgroup,
        Shape(8),
        Some(Cfg(1)),
    );
    for other in [other_instance, other_platform, other_shape] {
        assert_eq!(estimate(&s, &other), Err(Fallback::Missing));
        assert_eq!(floor(&s, &other), 0);
    }
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

// ---- M1: never a whole host ------------------------------------------------------------

fn on_host(host: u64) -> Bounds {
    Bounds {
        host_memory: host,
        ..Bounds::default()
    }
}

/// M1: a key floor stays strictly below the host's memory, on a small host as on a
/// large one.
#[test]
fn a_floor_stays_strictly_below_the_host() {
    for (host, expect) in [
        (16 * GIB, 16 * GIB - 256 * MIB),
        (8 * GIB + 1, 8 * GIB),
        (8 * GIB, 8 * GIB - 256 * MIB),
    ] {
        let mut s = Summaries::default();
        let k = linux(1);
        let b = on_host(host);
        s.observe(&k, &kill(4 * GIB, 6 * GIB, Some(64 * GIB)), DAY, &b);
        assert_eq!(s.key_floor(&k, &b), expect, "host {host}");
        assert!(s.key_floor(&k, &b) < host);
    }
}

/// M1: an estimate that would reach the host is `OverCap`, not a clamped whole-host
/// booking; one step below is fine.
#[test]
fn an_estimate_reaching_the_host_is_over_cap() {
    let b = on_host(8 * GIB);
    let ask = |peak| {
        let mut s = Summaries::default();
        let k = linux(1);
        for _ in 0..5 {
            s.observe(&k, &success(peak), DAY, &b);
        }
        s.estimate(&k, DAY, &b, &Hint::default())
    };
    // 6.5 GiB x 1.2 + 128 MiB = 7.925 GiB -> 8 GiB: the whole host.
    assert_eq!(ask(6 * GIB + GIB / 2), Err(Fallback::OverCap));
    assert_eq!(ask(6 * GIB).unwrap().memory_bytes, 7680 * MIB);
    // A host smaller than the minimum estimate cannot be sized at all.
    let tiny = on_host(GIB);
    let mut s = Summaries::default();
    let k = linux(1);
    for _ in 0..5 {
        s.observe(&k, &success(GIB / 4), DAY, &tiny);
    }
    assert_eq!(
        s.estimate(&k, DAY, &tiny, &Hint::default()),
        Err(Fallback::OverCap)
    );
}

/// M1 (departure 7): a hint that would reach the host is `OverCap` too: the crate never
/// returns a whole-host booking, whoever asked for it. A hint just under is passed.
#[test]
fn a_hint_never_reaches_the_host() {
    let mut s = Summaries::default();
    let k = linux(1);
    let b = on_host(8 * GIB);
    for _ in 0..5 {
        s.observe(&k, &success(GIB), DAY, &b);
    }
    let ask = |memory_bytes| {
        s.estimate(
            &k,
            DAY,
            &b,
            &Hint {
                memory_bytes: Some(memory_bytes),
                cpu_millis: None,
            },
        )
    };
    assert_eq!(ask(8 * GIB), Err(Fallback::OverCap));
    assert_eq!(ask(12 * GIB), Err(Fallback::OverCap));
    assert_eq!(ask(8 * GIB - 1).unwrap().memory_bytes, 8 * GIB - 1);
}

// ---- 2.4 hints --------------------------------------------------------------------------

/// Rule 2.4: the client's hint is never lowered, for memory and CPU.
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
    assert_eq!(ask(None, None).memory_bytes, GIB);
}

// ---- 3.3 CPU -----------------------------------------------------------------------------

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
