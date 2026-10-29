//! The counts of a reading's lists and the conversion of a reading into the scheduler's load.

use kbf_proto::LoadDrops;
use kbf_proto::worker::{LeaseClock as WireClock, LeaseMinute as WireMinute};

use super::*;

fn wire(term: u64, seq: u64) -> WireLease {
    WireLease { term, seq }
}

/// A reading with every field set to a number of its own, so a field copied to the wrong
/// place or left at its default shows.
fn full_load() -> NodeLoad {
    NodeLoad {
        seq: 11,
        mono_usec: 12,
        cpu_usage_usec: 13,
        memory_working_set_bytes: 14,
        covered: vec![wire(2, 3), wire(2, 4)],
        actions_cpu_limited: true,
        cpu_throttled_usec: Some(15),
        cpu_pressure_some_total_usec: Some(16),
        memory_active_file_bytes: 17,
        lease_clocks: vec![WireClock {
            lease: Some(wire(2, 3)),
            started_mono_usec: 5,
        }],
        minutes: vec![WireMinute {
            lease: Some(wire(2, 4)),
            minute: 6,
            cpu_mean_millis: 7,
            cpu_peak_window_millis: 8,
            ws_max_bytes: 9,
        }],
    }
}

/// Asserts that every field of a reading reaches the scheduler's load, each on its own field,
/// with the lease ids of the scheduler's own type: the scalars, the telemetry a daemon sent,
/// `covered`, each clock and each minute. The lists are the vetted ones, so a record the vet
/// dropped is not in it even though the reading carries it.
#[test]
fn a_reading_reaches_the_scheduler_field_by_field() {
    let mut load = full_load();
    let loose = WireMinute {
        lease: None,
        ..load.minutes[0]
    };
    load.minutes.push(loose);
    let vetted = load.vet();
    let fed = sched_load(&load, &vetted);
    let expected = usage::NodeLoad {
        seq: 11,
        mono_usec: 12,
        cpu_usage_usec: 13,
        memory_working_set_bytes: 14,
        memory_active_file_bytes: 17,
        covered: vec![LeaseId::new(2, 3), LeaseId::new(2, 4)],
        actions_cpu_limited: true,
        cpu_throttled_usec: 15,
        cpu_pressure_some_total_usec: 16,
        lease_clocks: vec![LeaseClock {
            lease: LeaseId::new(2, 3),
            started_mono_usec: 5,
        }],
        minutes: vec![LeaseMinute {
            lease: LeaseId::new(2, 4),
            minute: 6,
            cpu_mean_millis: 7,
            cpu_peak_window_millis: 8,
            ws_max_bytes: 9,
        }],
    };
    assert_eq!(fed, expected);
}

/// Asserts that telemetry the daemon could not read, which the scheduler holds as a plain
/// number, is 0, and that a reading with no lists gives a load with none.
#[test]
fn telemetry_a_daemon_did_not_send_reads_as_zero() {
    let load = NodeLoad {
        seq: 1,
        cpu_throttled_usec: None,
        cpu_pressure_some_total_usec: None,
        ..NodeLoad::default()
    };
    let fed = sched_load(&load, &load.vet());
    assert_eq!(fed.cpu_throttled_usec, 0);
    assert_eq!(fed.cpu_pressure_some_total_usec, 0);
    assert_eq!(fed.seq, 1);
    assert!(fed.covered.is_empty());
    assert!(fed.lease_clocks.is_empty());
    assert!(fed.minutes.is_empty());
}

/// Asserts that a clock or a minute naming no lease is left out of the converted load rather
/// than given a lease of 0.0, though `vet` leaves none in a `Vetted` it makes.
#[test]
fn a_record_that_names_no_lease_is_left_out() {
    let load = full_load();
    let loose = WireMinute {
        lease: None,
        ..load.minutes[0]
    };
    let vetted = Vetted {
        clocks: vec![
            WireClock {
                lease: None,
                started_mono_usec: 1,
            },
            load.lease_clocks[0],
        ],
        minutes: vec![loose, load.minutes[0]],
        ..Vetted::default()
    };
    let fed = sched_load(&load, &vetted);
    assert_eq!(fed.lease_clocks.len(), 1);
    assert_eq!(fed.lease_clocks[0].lease, LeaseId::new(2, 3));
    assert_eq!(fed.minutes.len(), 1);
    assert_eq!(fed.minutes[0].lease, LeaseId::new(2, 4));
}

/// Asserts that the counts add what each reading kept and dropped, that each reason has its
/// own count in the view, and that a count at its limit stays there.
#[test]
fn the_counts_add_up_and_saturate() {
    let vetted = Vetted {
        clocks: vec![WireClock::default(); 2],
        minutes: vec![WireMinute::default(); 3],
        drops: LoadDrops {
            invalid: 4,
            duplicate: 5,
            over_cap: 6,
        },
        ..Vetted::default()
    };
    let mut lists = Lists::default();
    lists.took(&vetted);
    lists.took(&vetted);
    lists.refused(7);
    let view = lists.view();
    assert_eq!((view.clocks, view.minutes), (4, 6));
    assert_eq!(
        (
            view.dropped.invalid,
            view.dropped.duplicate,
            view.dropped.over_cap
        ),
        (8, 10, 12)
    );
    assert_eq!(view.refused_minutes, 7);

    let mut full = Lists {
        clocks: u64::MAX,
        minutes: u64::MAX,
        invalid: u64::MAX,
        duplicate: u64::MAX,
        over_cap: u64::MAX,
        refused_minutes: u64::MAX,
    };
    full.took(&vetted);
    full.refused(1);
    assert_eq!(
        full,
        Lists {
            clocks: u64::MAX,
            minutes: u64::MAX,
            invalid: u64::MAX,
            duplicate: u64::MAX,
            over_cap: u64::MAX,
            refused_minutes: u64::MAX,
        }
    );
}
