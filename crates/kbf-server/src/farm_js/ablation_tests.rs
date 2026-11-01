//! The ablation counters ([`super::ablation`]) with no farm: which charge is which booking,
//! which reason of giving a lease up counts as a retry, how the scheduler's counts split into
//! the placement cases, and the text written.

use kbf_sched::{LoadCounters, RequeueReason};
use kbf_types::LeaseKind;
use kbf_types::sched::usage::LeaseCost;

use super::ablation::{Ablation, Booking};

const GIB: u64 = 1 << 30;

fn cost(memory_bytes: u64, cold: bool, retry: bool) -> LeaseCost {
    LeaseCost {
        cpu_millis: 500,
        memory_bytes,
        limit_bytes: 4 * GIB,
        cold,
        retry,
        key_hash: 7,
    }
}

/// Asserts each charge lands in its own source and in no other: a learned estimate is a
/// prediction, the cold costs are cold, a rung rerun is a retry even when the key is cold, no
/// memory charged is the default, a lease with no recorded cost is the default, and a VM,
/// which books its memory, is never a prediction. Catches a source decided by the memory alone
/// (a VM or a retry read as a prediction), and a cold key read as learned.
#[test]
fn a_charge_is_booked_to_its_own_source() {
    let action = LeaseKind::Action;
    let cases = [
        (
            action,
            Some(cost(2 * GIB, false, false)),
            Booking::Prediction,
        ),
        (action, Some(cost(4 * GIB, true, false)), Booking::Cold),
        (action, Some(cost(16 * GIB, false, true)), Booking::Retry),
        (action, Some(cost(16 * GIB, true, true)), Booking::Retry),
        (action, Some(cost(0, false, false)), Booking::Default),
        (action, None, Booking::Default),
        (
            LeaseKind::Vm,
            Some(cost(4 * GIB, false, false)),
            Booking::Default,
        ),
    ];
    for (kind, charged, want) in cases {
        assert_eq!(Booking::of(kind, charged), want, "{kind:?} {charged:?}");
    }
}

/// Asserts one booking moves one source: after a booking of each source in turn, that source
/// is one and the others are what they were.
#[test]
fn a_booking_moves_its_own_source_only() {
    let all = [
        Booking::Prediction,
        Booking::Cold,
        Booking::Retry,
        Booking::Default,
    ];
    for booking in all {
        let mut ablation = Ablation::default();
        ablation.booked(booking);
        for other in all {
            assert_eq!(
                ablation.bookings(other),
                u64::from(other == booking),
                "{booking:?} moved {other:?}"
            );
        }
    }
}

/// Asserts only the kill of an action past its own memory limit is a retry, counted under the
/// whole GiB it runs again at, and every other reason a lease is given up moves nothing.
/// Catches a node-pressure requeue (the same booking again, no rung) counted as a ladder
/// climb.
#[test]
fn only_an_own_limit_kill_is_an_oom_retry_and_it_counts_by_rung() {
    let mut ablation = Ablation::default();
    for reason in [
        RequeueReason::Silent,
        RequeueReason::Replaced,
        RequeueReason::Reconnected,
        RequeueReason::NotStarted,
        RequeueReason::SessionSwitch,
        RequeueReason::NodeMemoryPressure,
    ] {
        ablation.requeued(reason);
    }
    assert_eq!(ablation, Ablation::default());
    let oom = |booked, raised| RequeueReason::OutOfMemory { booked, raised };
    ablation.requeued(oom(4 * GIB, 16 * GIB));
    ablation.requeued(oom(4 * GIB, 16 * GIB));
    ablation.requeued(oom(16 * GIB, 32 * GIB));
    assert_eq!(ablation.oom_retries(16), 2);
    assert_eq!(ablation.oom_retries(32), 1);
    assert_eq!(ablation.oom_retries(4), 0);
}

/// Asserts the scheduler's counts split into three disjoint cases, each moved by its own
/// field: an admission is by load unless it is a fallback, a fallback is not counted by load,
/// and a blocked request is neither. Catches `by_load` taken as all admissions (the fallback
/// counted twice) and a blocked count folded into either.
#[test]
fn the_load_counts_split_into_disjoint_placement_cases() {
    let split = |counters: LoadCounters| Ablation::placements(&counters);
    assert_eq!(split(LoadCounters::default()), [0, 0, 0]);
    let admitted = LoadCounters {
        admitted: 3,
        ..LoadCounters::default()
    };
    assert_eq!(split(admitted), [3, 0, 0]);
    let fallback = LoadCounters {
        admitted: 3,
        admitted_fallback: 2,
        ..LoadCounters::default()
    };
    assert_eq!(split(fallback), [1, 2, 0]);
    let blocked = LoadCounters {
        blocked: 5,
        ..LoadCounters::default()
    };
    assert_eq!(split(blocked), [0, 0, 5]);
}

/// Asserts the text: every family is written from the start with its fixed series at 0, the
/// retry family has a series only for a rung that was retried at, and the counts are the ones
/// given.
#[test]
fn the_families_are_written_from_the_start_and_carry_the_counts() {
    let mut empty = String::new();
    Ablation::default().render(&mut empty, &LoadCounters::default());
    let samples = |text: &str| -> Vec<String> {
        text.lines()
            .filter(|l| !l.starts_with('#'))
            .map(str::to_owned)
            .collect()
    };
    let zero = samples(&empty);
    assert_eq!(zero.len(), 3 + 3 + 4, "{zero:?}");
    assert!(zero.iter().all(|l| l.ends_with(" 0")), "{zero:?}");

    let mut ablation = Ablation::default();
    ablation.booked(Booking::Retry);
    ablation.requeued(RequeueReason::OutOfMemory {
        booked: 4 * GIB,
        raised: 16 * GIB,
    });
    let counters = LoadCounters {
        admitted: 4,
        admitted_fallback: 1,
        blocked: 6,
        profile_cpu: 2,
        profile_memory: 3,
        curve_flat: 5,
        ..LoadCounters::default()
    };
    let mut text = String::new();
    ablation.render(&mut text, &counters);
    let lines = samples(&text);
    for want in [
        "kbf_ablation_placements_total{decision=\"by_load\"} 3",
        "kbf_ablation_placements_total{decision=\"fallback_charge\"} 1",
        "kbf_ablation_placements_total{decision=\"refused_over_capacity\"} 6",
        "kbf_ablation_charges_total{source=\"profile_cpu\"} 2",
        "kbf_ablation_charges_total{source=\"profile_memory\"} 3",
        "kbf_ablation_charges_total{source=\"curve_flat\"} 5",
        "kbf_ablation_bookings_total{source=\"retry\"} 1",
        "kbf_ablation_bookings_total{source=\"default\"} 0",
        "kbf_ablation_oom_retries_total{rung_gib=\"16\"} 1",
    ] {
        assert!(lines.iter().any(|l| l == want), "{want} in {lines:?}");
    }
    assert_eq!(lines.len(), 3 + 3 + 4 + 1, "{lines:?}");
}
