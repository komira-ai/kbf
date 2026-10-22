use std::collections::BTreeMap;
use std::time::Duration;

use kbf_proto::worker::UsageBasis;

use super::*;

const MIB: u64 = 1 << 20;

fn life(max_mib: u64, user: u64, system: u64) -> Lifetime {
    Lifetime {
        max_footprint: max_mib * MIB,
        user_ticks: user,
        system_ticks: system,
    }
}

/// A fake process table: `read` gives the pid's current row.
fn table(rows: &[(i32, u64, Lifetime)]) -> impl Fn(i32) -> Option<Rusage> {
    let rows: BTreeMap<i32, Rusage> = rows
        .iter()
        .map(|&(pid, footprint, l)| {
            (
                pid,
                Rusage {
                    footprint,
                    lifetime: Some(l),
                },
            )
        })
        .collect();
    move |pid| rows.get(&pid).copied()
}

fn footprint(usage: &ResourceUsage) -> FootprintUsage {
    usage.footprint.expect("a footprint block")
}

const SECOND: Duration = Duration::from_secs(1);
const ONE_TO_ONE: (u32, u32) = (1, 1);

/// Catches: a peak that is the largest process, or the largest sum of footprints at one
/// poll (a polled sample), instead of the sum of each process's lifetime maximum. Two
/// processes whose maxima were at different times (their footprints now are small) sum;
/// mutants seen red: `sum` replaced by `max` of the maxima (peak 300 MiB, not 500),
/// and `max_footprint` replaced by the footprint now (peak 30 MiB, not 500).
#[test]
fn the_peak_sums_each_processs_lifetime_maximum_not_a_polled_footprint() {
    let mut meter = Meter::default();
    let now = meter.poll(
        [10, 11],
        table(&[
            (10, 10 * MIB, life(300, 0, 0)),
            (11, 20 * MIB, life(200, 0, 0)),
        ]),
    );
    assert_eq!(now, 30 * MIB, "the poll still returns the footprints now");
    let usage = meter.usage(SECOND, ONE_TO_ONE);
    assert_eq!(footprint(&usage).peak_footprint_bytes, Some(500 * MIB));
    assert_eq!(usage.peak_memory_bytes, 500 * MIB);
}

/// Catches: CPU added up over polls (a process read three times counted three times),
/// or the first reading kept. The second poll's counters replace the first's; mutants
/// seen red: `record` adding to the earlier reading (user 3x too large), and `record`
/// ignoring a later reading (user stuck at the first).
#[test]
fn cpu_is_the_last_reading_of_each_process_not_a_sum_of_readings() {
    let mut meter = Meter::default();
    meter.poll([10], table(&[(10, MIB, life(1, 1_000_000, 500_000))]));
    meter.poll([10], table(&[(10, MIB, life(1, 3_000_000, 1_000_000))]));
    meter.poll(
        [10, 11],
        table(&[
            (10, MIB, life(1, 3_000_000, 1_000_000)),
            (11, MIB, life(1, 2_000_000, 0)),
        ]),
    );
    let f = footprint(&meter.usage(SECOND, ONE_TO_ONE));
    // 1 tick = 1 ns here: 5_000_000 ns = 5000 us.
    assert_eq!(f.cpu_user_micros, Some(5000));
    assert_eq!(f.cpu_system_micros, Some(1000));
}

/// Catches: a process that exits between polls dropping out of the totals (the usage
/// would shrink when its pid stops answering). Mutant seen red: `poll` clearing the
/// pids `read` returns nothing for.
#[test]
fn an_exited_process_keeps_what_it_was_last_read_with() {
    let mut meter = Meter::default();
    meter.poll(
        [10, 11],
        table(&[
            (10, MIB, life(40, 2_000_000, 0)),
            (11, MIB, life(60, 1_000_000, 0)),
        ]),
    );
    // Pid 11 is gone at the next poll, and adds nothing to the footprint now.
    let now = meter.poll([10, 11], table(&[(10, 2 * MIB, life(40, 4_000_000, 0))]));
    assert_eq!(now, 2 * MIB);
    let f = footprint(&meter.usage(SECOND, ONE_TO_ONE));
    assert_eq!(f.peak_footprint_bytes, Some(100 * MIB));
    assert_eq!(f.cpu_user_micros, Some(5000));
}

/// Catches: a pid the kernel handed to a new process within the lease replacing the
/// old process's totals (its memory and CPU lost). Mutant seen red: `record` without the
/// regression check.
#[test]
fn a_reused_pid_keeps_the_process_that_held_it_before() {
    let mut meter = Meter::default();
    meter.record(10, life(100, 4_000_000, 0));
    meter.record(10, life(30, 1_000_000, 0));
    let f = footprint(&meter.usage(SECOND, ONE_TO_ONE));
    assert_eq!(f.peak_footprint_bytes, Some(130 * MIB));
    assert_eq!(f.cpu_user_micros, Some(5000));
    // A lower CPU time alone is a different process too.
    let mut meter = Meter::default();
    meter.record(10, life(100, 4_000_000, 0));
    meter.record(10, life(100, 1_000_000, 0));
    assert_eq!(
        footprint(&meter.usage(SECOND, ONE_TO_ONE)).peak_footprint_bytes,
        Some(200 * MIB)
    );
}

/// Catches: mach ticks read as nanoseconds (Apple Silicon's timebase is 125/3, so CPU
/// would be 41.7 times too small), a divide before the multiply losing the fraction,
/// a zero denominator panicking, and an overflow wrapping. Mutants seen red:
/// numer and denom swapped; `denom.max(1)` removed (divide by zero).
#[test]
fn ticks_become_microseconds_with_the_machines_timebase() {
    // 24 MHz ticks on Apple Silicon: 24_000_000 ticks is one second.
    assert_eq!(ticks_to_micros(24_000_000, (125, 3)), 1_000_000);
    assert_eq!(
        ticks_to_micros(3, (125, 3)),
        0,
        "125 ns is under a microsecond"
    );
    assert_eq!(ticks_to_micros(9, (125, 3)), 0);
    assert_eq!(ticks_to_micros(24, (125, 3)), 1);
    assert_eq!(
        ticks_to_micros(5_000_000, (1, 1)),
        5000,
        "Intel: a tick is a nanosecond"
    );
    assert_eq!(
        ticks_to_micros(1000, (1, 0)),
        1,
        "a zero denominator reads as 1"
    );
    assert_eq!(ticks_to_micros(u64::MAX, (u32::MAX, 1)), u64::MAX);
}

/// Catches: the usage claiming a cgroup or no basis, an unread peak reported as zero
/// bytes ("absent is not zero"), and a lease with no readable process losing its wall
/// time. Mutants seen red: `basis` left unspecified; `read.then_some` replaced by
/// `Some`; `cgroup: Some(default)`.
#[test]
fn nothing_read_reports_only_the_wall_time_under_the_footprint_basis() {
    let usage = Meter::default().usage(Duration::from_millis(1500), ONE_TO_ONE);
    assert_eq!(usage.basis(), UsageBasis::MacFootprint);
    assert_eq!(usage.cgroup, None);
    assert_eq!(
        footprint(&usage),
        FootprintUsage {
            peak_footprint_bytes: None,
            cpu_user_micros: None,
            cpu_system_micros: None,
            wall_micros: Some(1_500_000),
        }
    );
    assert_eq!(usage.wall_micros, 1_500_000);
    assert_eq!((usage.peak_memory_bytes, usage.cpu_user_micros), (0, 0));
}

/// Catches: a read that gave no lifetime counters (a platform without them) still
/// being counted as a read process, which would report zero peak instead of absent.
#[test]
fn a_poll_without_lifetime_counters_reads_the_footprint_and_records_nothing() {
    let mut meter = Meter::default();
    let now = meter.poll([10], |_| {
        Some(Rusage {
            footprint: 7,
            lifetime: None,
        })
    });
    assert_eq!(now, 7);
    assert_eq!(
        footprint(&meter.usage(SECOND, ONE_TO_ONE)).peak_footprint_bytes,
        None
    );
}

/// Catches: the platform gate. Off macOS the native driver reports no usage (it has no
/// cgroup and no footprint counters, and must not invent a basis); on macOS it does.
#[test]
fn the_report_is_made_on_macos_only() {
    let meter = Mutex::new(Meter::default());
    let report = report(&meter, SECOND);
    assert_eq!(report.is_some(), cfg!(target_os = "macos"));
}
