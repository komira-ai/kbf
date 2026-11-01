//! `NodeLoad::vet`: which records of a load's three lists a receiver keeps, what it counts as
//! dropped, and that no load, however bad, costs more than its own records.

use kbf_proto::worker::{LeaseClock, LeaseId, LeaseMinute, NodeLoad};
use kbf_proto::{LoadDrops, MAX_LOAD_LEASES, MAX_LOAD_MINUTES, MAX_MEMORY_BYTES, TAIL_MINUTE};

/// The reading's own clock in every load below.
const MONO: u64 = 1_000_000;

fn lease(term: u64, seq: u64) -> LeaseId {
    LeaseId { term, seq }
}

/// A valid minute of the lease `(term, seq)`.
fn minute(term: u64, seq: u64, index: u32) -> LeaseMinute {
    LeaseMinute {
        lease: Some(lease(term, seq)),
        minute: index,
        cpu_mean_millis: 100,
        cpu_peak_window_millis: 200,
        ws_max_bytes: 4096,
    }
}

/// A valid clock of the lease `(term, seq)`.
fn clock(term: u64, seq: u64, started: u64) -> LeaseClock {
    LeaseClock {
        lease: Some(lease(term, seq)),
        started_mono_usec: started,
    }
}

fn load(
    covered: Vec<LeaseId>,
    lease_clocks: Vec<LeaseClock>,
    minutes: Vec<LeaseMinute>,
) -> NodeLoad {
    NodeLoad {
        seq: 1,
        mono_usec: MONO,
        covered,
        lease_clocks,
        minutes,
        ..NodeLoad::default()
    }
}

fn drops(invalid: u32, duplicate: u32, over_cap: u32) -> LoadDrops {
    LoadDrops {
        invalid,
        duplicate,
        over_cap,
    }
}

/// `n` distinct leases, in order.
fn leases(n: usize) -> Vec<LeaseId> {
    (0..n as u64).map(|i| lease(1, i)).collect()
}

/// A load that passes whole: every list non-empty, nothing dropped, nothing reordered.
#[test]
fn an_honest_load_passes_untouched() {
    let covered = vec![lease(1, 1), lease(1, 2), lease(2, 1)];
    let clocks = vec![clock(1, 1, 0), clock(1, 2, 10), clock(2, 1, MONO)];
    let minutes = vec![minute(1, 1, 0), minute(1, 1, 1), minute(2, 1, 0)];
    let vetted = load(covered.clone(), clocks.clone(), minutes.clone()).vet();
    assert_eq!(vetted.covered, covered);
    assert_eq!(vetted.clocks, clocks);
    assert_eq!(vetted.minutes, minutes);
    assert_eq!(vetted.drops, LoadDrops::default());
    assert_eq!(vetted.drops.total(), 0);
}

/// The edges of a minute record: 63 is kept and 64 dropped; a working set at the cap is kept and
/// one byte over dropped; a mean equal to its peak is kept and one above dropped; a minute with
/// no lease is dropped. The records around a dropped one are kept.
#[test]
fn minute_bounds() {
    let tail = LeaseMinute {
        minute: TAIL_MINUTE,
        ..minute(1, 2, 0)
    };
    let past_tail = LeaseMinute {
        minute: TAIL_MINUTE + 1,
        ..minute(1, 3, 0)
    };
    let ws_at_cap = LeaseMinute {
        ws_max_bytes: MAX_MEMORY_BYTES,
        ..minute(1, 4, 0)
    };
    let ws_over = LeaseMinute {
        ws_max_bytes: MAX_MEMORY_BYTES + 1,
        ..minute(1, 5, 0)
    };
    let mean_is_peak = LeaseMinute {
        cpu_mean_millis: 200,
        ..minute(1, 6, 0)
    };
    let mean_over_peak = LeaseMinute {
        cpu_mean_millis: 201,
        ..minute(1, 7, 0)
    };
    let no_lease = LeaseMinute {
        lease: None,
        ..minute(1, 8, 0)
    };
    let first = minute(1, 1, 0);
    let last = minute(1, 9, 0);
    let sent = vec![
        first,
        tail,
        past_tail,
        ws_at_cap,
        ws_over,
        mean_is_peak,
        mean_over_peak,
        no_lease,
        last,
    ];
    let vetted = load(Vec::new(), Vec::new(), sent).vet();
    assert_eq!(
        vetted.minutes,
        vec![first, tail, ws_at_cap, mean_is_peak, last]
    );
    assert_eq!(vetted.drops, drops(4, 0, 0));
}

/// The edges of a clock record: a start at the reading is kept (and one at 0), one microsecond
/// after it dropped; a clock with no lease is dropped.
#[test]
fn clock_bounds() {
    let sent = vec![
        clock(1, 1, MONO),
        clock(1, 2, MONO + 1),
        LeaseClock {
            lease: None,
            started_mono_usec: 5,
        },
        clock(1, 3, 0),
    ];
    let vetted = load(Vec::new(), sent, Vec::new()).vet();
    assert_eq!(vetted.clocks, vec![clock(1, 1, MONO), clock(1, 3, 0)]);
    assert_eq!(vetted.drops, drops(2, 0, 0));
}

/// What makes two records the same: a minute is its lease and its index, a clock its lease. The
/// first of a pair stays. The lease is told by term and sequence both.
#[test]
fn duplicates_use_the_right_key() {
    let a1 = minute(1, 5, 1);
    let a1_again = LeaseMinute {
        ws_max_bytes: 9999,
        ..a1
    };
    let a2 = minute(1, 5, 2);
    let same_seq_other_term = minute(2, 5, 1);
    let same_term_other_seq = minute(1, 6, 1);
    let vetted = load(
        Vec::new(),
        Vec::new(),
        vec![a1, a1_again, a2, same_seq_other_term, same_term_other_seq],
    )
    .vet();
    assert_eq!(
        vetted.minutes,
        vec![a1, a2, same_seq_other_term, same_term_other_seq]
    );
    assert_eq!(vetted.drops, drops(0, 1, 0));

    let vetted = load(
        Vec::new(),
        vec![
            clock(1, 5, 10),
            clock(1, 5, 20),
            clock(2, 5, 30),
            clock(1, 6, 40),
        ],
        Vec::new(),
    )
    .vet();
    assert_eq!(
        vetted.clocks,
        vec![clock(1, 5, 10), clock(2, 5, 30), clock(1, 6, 40)]
    );
    assert_eq!(vetted.drops, drops(0, 1, 0));
}

/// A record that breaks a rule is invalid even when it also repeats an earlier one, and it does
/// not take the place of a valid record that follows it.
#[test]
fn an_invalid_record_is_not_a_duplicate_and_holds_no_key() {
    let good = minute(1, 1, 0);
    let bad_repeat = LeaseMinute {
        cpu_mean_millis: 500,
        ..good
    };
    let vetted = load(Vec::new(), Vec::new(), vec![good, bad_repeat]).vet();
    assert_eq!(vetted.minutes, vec![good]);
    assert_eq!(vetted.drops, drops(1, 0, 0));

    let vetted = load(Vec::new(), Vec::new(), vec![bad_repeat, good]).vet();
    assert_eq!(vetted.minutes, vec![good]);
    assert_eq!(vetted.drops, drops(1, 0, 0));
}

/// A list over its cap is cut to it, not refused: at the cap and one past it, and a hundred
/// thousand records past it. Each list has its own cap.
#[test]
fn lists_over_the_cap_are_cut_not_refused() {
    for n in [MAX_LOAD_LEASES, MAX_LOAD_LEASES + 1, 100_000] {
        let over = u32::try_from(n - MAX_LOAD_LEASES).expect("small");
        let ids = leases(n);

        let vetted = load(ids.clone(), Vec::new(), Vec::new()).vet();
        assert_eq!(vetted.covered, ids[..MAX_LOAD_LEASES]);
        assert_eq!(vetted.drops, drops(0, 0, over));

        let clocks: Vec<LeaseClock> = ids.iter().map(|id| clock(id.term, id.seq, 0)).collect();
        let vetted = load(Vec::new(), clocks.clone(), Vec::new()).vet();
        assert_eq!(vetted.clocks, clocks[..MAX_LOAD_LEASES]);
        assert_eq!(vetted.drops, drops(0, 0, over));
    }
    for n in [MAX_LOAD_MINUTES, MAX_LOAD_MINUTES + 1, 100_000] {
        let over = u32::try_from(n - MAX_LOAD_MINUTES).expect("small");
        let minutes: Vec<LeaseMinute> = (0..n as u64).map(|i| minute(1, i, 0)).collect();
        let vetted = load(Vec::new(), Vec::new(), minutes.clone()).vet();
        assert_eq!(vetted.minutes, minutes[..MAX_LOAD_MINUTES]);
        assert_eq!(vetted.drops, drops(0, 0, over));
    }
}

/// A record past the cap is counted as over the cap and nothing else, whatever it is: an
/// invalid one and a repeat of a kept one are not also judged.
#[test]
fn the_excess_is_counted_once_and_never_judged() {
    let mut minutes: Vec<LeaseMinute> = (0..MAX_LOAD_MINUTES as u64)
        .map(|i| minute(1, i, 0))
        .collect();
    minutes.push(LeaseMinute {
        lease: None,
        ..minute(1, 0, 0)
    });
    minutes.push(minute(1, 0, 0));
    let vetted = load(Vec::new(), Vec::new(), minutes).vet();
    assert_eq!(vetted.minutes.len(), MAX_LOAD_MINUTES);
    assert_eq!(vetted.drops, drops(0, 0, 2));
}

/// The drops of the three lists add up in one count, and the sum stops at the largest number
/// instead of wrapping.
#[test]
fn total_saturates() {
    assert_eq!(drops(1, 2, 3).total(), 6);
    assert_eq!(drops(u32::MAX, 1, 1).total(), u32::MAX);
    assert_eq!(drops(1, u32::MAX, u32::MAX).total(), u32::MAX);

    let vetted = load(
        vec![lease(1, 1), lease(1, 1)],
        vec![clock(1, 1, MONO + 1)],
        vec![minute(1, 1, 64)],
    )
    .vet();
    assert_eq!(vetted.drops, drops(2, 1, 0));
    assert_eq!(vetted.drops.total(), 3);
}

/// `covered` has a cap and repeats but no rule of its own: any lease is a lease, the first
/// of a repeat stays, the order is kept.
#[test]
fn covered_is_cut_and_deduplicated() {
    let ids = leases(MAX_LOAD_LEASES + 4);
    let vetted = load(ids.clone(), Vec::new(), Vec::new()).vet();
    assert_eq!(vetted.covered, ids[..MAX_LOAD_LEASES]);
    assert_eq!(vetted.drops, drops(0, 0, 4));

    let sent = vec![
        lease(3, 3),
        lease(1, 1),
        lease(3, 3),
        lease(0, 0),
        lease(1, 1),
    ];
    let vetted = load(sent, Vec::new(), Vec::new()).vet();
    assert_eq!(vetted.covered, vec![lease(3, 3), lease(1, 1), lease(0, 0)]);
    assert_eq!(vetted.drops, drops(0, 2, 0));

    // The same sequence under two terms is two leases.
    let vetted = load(vec![lease(1, 1), lease(2, 1)], Vec::new(), Vec::new()).vet();
    assert_eq!(vetted.covered, vec![lease(1, 1), lease(2, 1)]);
    assert_eq!(vetted.drops, drops(0, 0, 0));
}

/// Vetting what vet returned drops nothing and changes nothing, so a record that passed once
/// passes again wherever the load is handed on. The load itself is not changed by vetting.
#[test]
fn vet_is_idempotent() {
    let messy = load(
        vec![lease(1, 1), lease(1, 1), lease(1, 2)],
        vec![
            clock(1, 1, 5),
            clock(1, 1, 6),
            clock(1, 2, MONO + 1),
            clock(1, 3, MONO),
        ],
        vec![
            minute(1, 1, 0),
            minute(1, 1, 0),
            minute(1, 1, 64),
            LeaseMinute {
                lease: None,
                ..minute(1, 2, 0)
            },
            minute(1, 1, 1),
        ],
    );
    let before = messy.clone();
    let once = messy.vet();
    assert_eq!(messy, before);
    assert_ne!(once.drops, LoadDrops::default());

    let again = NodeLoad {
        covered: once.covered.clone(),
        lease_clocks: once.clocks.clone(),
        minutes: once.minutes.clone(),
        ..messy
    }
    .vet();
    assert_eq!(again.covered, once.covered);
    assert_eq!(again.clocks, once.clocks);
    assert_eq!(again.minutes, once.minutes);
    assert_eq!(again.drops, LoadDrops::default());
}

/// A load made only of bad records, far over every cap, is vetted to nothing and counted, and
/// vetting does not fail: the heartbeat that carried it is the receiver's to answer.
#[test]
fn a_hostile_load_vets_to_empty_and_is_counted() {
    let n = 100_000;
    let hostile = load(
        Vec::new(),
        (0..n)
            .map(|_| LeaseClock {
                lease: None,
                started_mono_usec: u64::MAX,
            })
            .collect(),
        (0..n)
            .map(|_| LeaseMinute {
                lease: None,
                minute: u32::MAX,
                cpu_mean_millis: u32::MAX,
                cpu_peak_window_millis: 0,
                ws_max_bytes: u64::MAX,
            })
            .collect(),
    );
    let vetted = hostile.vet();
    assert!(vetted.clocks.is_empty());
    assert!(vetted.minutes.is_empty());
    let cut = u32::try_from(n - MAX_LOAD_LEASES).expect("small")
        + u32::try_from(n - MAX_LOAD_MINUTES).expect("small");
    let judged = u32::try_from(MAX_LOAD_LEASES + MAX_LOAD_MINUTES).expect("small");
    assert_eq!(vetted.drops, drops(judged, 0, cut));
}

/// The numbers the receiver applies are the documented ones, and the same ones the job
/// scheduler's agent wire documents for the same lists (`js_agent_wire::limits`). This crate does
/// not depend on that one, so the numbers are pinned here as literals.
#[test]
fn the_cap_table_is_the_documented_one() {
    assert_eq!(MAX_LOAD_LEASES, 96);
    assert_eq!(MAX_LOAD_MINUTES, 64);
    assert_eq!(TAIL_MINUTE, 63);
    assert_eq!(MAX_MEMORY_BYTES, 1 << 45);
}
