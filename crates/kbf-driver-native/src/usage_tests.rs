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
        ..Lifetime::default()
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
/// catches: the sum folded with `max` instead (peak 300 MiB, not 500).
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
/// or the first reading kept. The second poll's counters replace the first's; catches `poll` not
/// recording its readings.
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
/// would shrink when its pid stops answering). Catches: `poll` not recording.
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
/// old process's totals (its memory and CPU lost). Catches: `record` without the
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
/// a zero denominator panicking, and an overflow wrapping. Catches:
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
/// time. Catches: `basis` left unspecified; `read.then_some` replaced by
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

/// Catches: a reused pid that is told only by its CPU time. The new process holds a
/// smaller high-water mark than the old one but has run at least as long (equal or more
/// CPU), so only the footprint clause of the check can see the reuse; without it the old
/// process's 100 MiB is overwritten and the peak reads 30 MiB. Catches: the
/// footprint clause dropped from `record`.
#[test]
fn a_reused_pid_with_a_smaller_peak_and_no_less_cpu_is_still_a_new_process() {
    for (new_cpu, want_user_us) in [(4_000_000, 8000), (6_000_000, 10_000)] {
        let mut meter = Meter::default();
        meter.record(10, life(100, 4_000_000, 0));
        meter.record(10, life(30, new_cpu, 0));
        let f = footprint(&meter.usage(SECOND, ONE_TO_ONE));
        assert_eq!(f.peak_footprint_bytes, Some(130 * MIB), "cpu {new_cpu}");
        assert_eq!(f.cpu_user_micros, Some(want_user_us), "cpu {new_cpu}");
    }
}

/// One process as a poll reads it: own CPU (ns, user only), CPU of the children it has
/// reaped (ns, user only) and its parent.
fn proc_row(own_ns: u64, child_ns: u64, ppid: i32) -> (u64, Lifetime) {
    let life = Lifetime {
        max_footprint: MIB,
        user_ticks: own_ns,
        child_user_ticks: child_ns,
        ppid,
        ..Lifetime::default()
    };
    (MIB, life)
}

fn poll(meter: &mut Meter, rows: &[(i32, (u64, Lifetime))]) {
    let rows: Vec<(i32, u64, Lifetime)> = rows.iter().map(|&(p, (f, l))| (p, f, l)).collect();
    let pids: Vec<i32> = rows.iter().map(|r| r.0).collect();
    meter.poll(pids, table(&rows));
}

const S: u64 = 1_000_000_000;

fn reap(meter: &mut Meter, leader: i32, user_secs: u64) {
    meter.reaped(
        leader,
        Reaped {
            user_micros: user_secs * 1_000_000,
            system_micros: 0,
        },
    );
}

fn user_micros(meter: &Meter) -> u64 {
    footprint(&meter.usage(SECOND, ONE_TO_ONE))
        .cpu_user_micros
        .expect("read")
}

/// Catches: a child that ran and was reaped between two polls, and the leader's tail
/// after its last poll, missing from the total. The polls saw the leader at 1 s and the
/// child at 0.1 s; the leader was reaped at 3 s (its own 1.2 s and the child's 1.8 s).
/// The total is the reaped figure.
#[test]
fn a_child_reaped_between_polls_and_the_leaders_tail_are_counted() {
    let mut meter = Meter::default();
    poll(
        &mut meter,
        &[(10, proc_row(S, 0, 1)), (11, proc_row(S / 10, 0, 10))],
    );
    poll(&mut meter, &[(10, proc_row(S, 0, 1))]);
    reap(&mut meter, 10, 3);
    assert_eq!(user_micros(&meter), 3_000_000);
}

/// Catches: a child counted twice, once from the poll that saw it alive and again inside
/// the leader's reaped total. The child was read at 2 s, then reaped by the leader
/// (reaped total 5 s: 1 s its own, 4 s the child's): the total is 5 s, not 7.
#[test]
fn a_child_polled_alive_and_then_reaped_counts_once() {
    let mut meter = Meter::default();
    poll(
        &mut meter,
        &[(10, proc_row(S, 0, 1)), (11, proc_row(2 * S, 0, 10))],
    );
    poll(&mut meter, &[(10, proc_row(S, 0, 1))]);
    reap(&mut meter, 10, 5);
    assert_eq!(user_micros(&meter), 5_000_000);
}

/// Catches: the CPU of a descendant that was never the leader's to reap dropped: an
/// orphan (its parent is launchd, pid 1) that was read at 2 s and exited before the
/// leader did is in no reaped total and is added.
#[test]
fn an_orphan_that_exited_before_the_reap_is_added() {
    let mut meter = Meter::default();
    poll(
        &mut meter,
        &[(10, proc_row(S, 0, 1)), (12, proc_row(2 * S, 0, 1))],
    );
    poll(&mut meter, &[(10, proc_row(S, 0, 1))]);
    reap(&mut meter, 10, 4);
    assert_eq!(user_micros(&meter), 6_000_000);
}

/// Catches: a process still running after the leader's reap (a server the action
/// started) missing: it is no part of the reaped total. It is read after the reap at
/// 1 s of its own and 0.5 s of children.
#[test]
fn a_process_read_after_the_reap_is_added_with_its_children() {
    let mut meter = Meter::default();
    poll(
        &mut meter,
        &[(10, proc_row(S, 0, 1)), (13, proc_row(S / 2, 0, 1))],
    );
    reap(&mut meter, 10, 2);
    poll(&mut meter, &[(13, proc_row(S, S / 2, 1))]);
    assert_eq!(user_micros(&meter), 3_500_000);
}

/// Catches: the sampled and the reaped figures added for a leader killed while its
/// children ran, and the reaped one alone taken when the children were not reaped. At the
/// last poll before the kill the leader held 1 s and its live child 4 s (5 s); the leader
/// was reaped at 1.5 s (the child was reaped by nobody it knows): the total is the
/// larger, 5 s. When the reap is the larger (6 s) it is the total.
#[test]
fn the_larger_of_the_reaped_and_the_last_poll_is_the_total_of_a_killed_leader() {
    for (reaped, want) in [(1, 5_000_000), (6, 6_000_000)] {
        let mut meter = Meter::default();
        poll(
            &mut meter,
            &[(10, proc_row(S, 0, 1)), (11, proc_row(4 * S, 0, 10))],
        );
        reap(&mut meter, 10, reaped);
        assert_eq!(user_micros(&meter), want, "reaped {reaped}");
    }
}

/// Catches: the children a live process has reaped (`ri_child_*`) left out of a lease
/// that never reaped its leader's figures (no reap recorded): the total is the last
/// poll's own and child CPU of each process, and a process that exited under a polled
/// parent is not added again.
#[test]
fn without_a_reap_the_total_is_the_last_polls_own_and_children_cpu() {
    let mut meter = Meter::default();
    poll(
        &mut meter,
        &[(10, proc_row(S, 0, 1)), (11, proc_row(2 * S, 0, 10))],
    );
    poll(&mut meter, &[(10, proc_row(S, 2 * S, 1))]);
    assert_eq!(user_micros(&meter), 3_000_000);
}
