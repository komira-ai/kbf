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

/// Catches: the leader's last reading added to its reaped total. The leader was read at
/// the first poll (1 s) and not at the second (it had exited, so the second poll found no
/// pid), then reaped at 3 s: its reading is inside the reaped figure, which is the whole
/// total, and the child that was alive at the second poll (2 s) is added once.
#[test]
fn a_leader_read_only_before_its_exit_is_counted_by_its_reap_alone() {
    let mut meter = Meter::default();
    poll(&mut meter, &[(10, proc_row(S, 0, 1))]);
    poll(&mut meter, &[(11, proc_row(2 * S, 0, 10))]);
    reap(&mut meter, 10, 3);
    assert_eq!(user_micros(&meter), 3_000_000);
}

/// Catches: a reap with no poll at all reporting nothing, or a peak invented for it. The
/// CPU is the reaped total (system time too) and the peak, which no poll read, is absent.
#[test]
fn a_reap_with_no_poll_reports_its_cpu_and_no_peak() {
    let mut meter = Meter::default();
    meter.reaped(
        10,
        Reaped {
            user_micros: 700,
            system_micros: 300,
        },
    );
    let f = footprint(&meter.usage(SECOND, ONE_TO_ONE));
    assert_eq!(f.cpu_user_micros, Some(700));
    assert_eq!(f.cpu_system_micros, Some(300));
    assert_eq!(f.peak_footprint_bytes, None);
}

/// Catches: a process that was never the leader's to reap and was still running at the
/// last poll dropped by the larger-of rule. The orphan (parent launchd, pid 1) was alive
/// at the last poll with 2 s; the leader's reap (4 s: the leader's own and the child it
/// reaped) is larger than the whole last poll (1 s + 2 s) and holds none of the orphan.
/// The total is the reap and the orphan, 6 s, not 4.
#[test]
fn an_orphan_alive_at_the_last_poll_is_added_to_the_reap() {
    let mut meter = Meter::default();
    poll(
        &mut meter,
        &[(10, proc_row(S, 0, 1)), (12, proc_row(2 * S, 0, 1))],
    );
    reap(&mut meter, 10, 4);
    assert_eq!(user_micros(&meter), 6_000_000);
}

/// Catches: the same orphan counted twice, or the larger-of rule lost for the leader's
/// own children: a child of the leader alive at the last poll (3 s) is part of the
/// reap (4 s), so the total is 4 s, with the orphan's 2 s on top.
#[test]
fn a_child_of_the_leader_alive_at_the_last_poll_is_not_added_with_the_orphan() {
    let mut meter = Meter::default();
    poll(
        &mut meter,
        &[
            (10, proc_row(S, 0, 1)),
            (11, proc_row(3 * S, 0, 10)),
            (12, proc_row(2 * S, 0, 1)),
        ],
    );
    reap(&mut meter, 10, 4);
    assert_eq!(user_micros(&meter), 6_000_000);
}

/// Catches: a grandchild alive at the last poll taken for an orphan because its parent is
/// not the leader: the parent is a live process of the same poll, so the grandchild is in
/// the chain (its parent will reap it) and the larger-of rule covers it.
#[test]
fn a_grandchild_alive_at_the_last_poll_is_in_the_chain() {
    let mut meter = Meter::default();
    poll(
        &mut meter,
        &[
            (10, proc_row(S, 0, 1)),
            (11, proc_row(S, 0, 10)),
            (13, proc_row(2 * S, 0, 11)),
        ],
    );
    reap(&mut meter, 10, 4);
    assert_eq!(user_micros(&meter), 4_000_000);
}

/// Catches: a parent read as unknown (0, when the table lookup lost the race with the
/// exit) replacing the parent an earlier read gave, which would make a child of a polled
/// parent look like an orphan and be counted again on top of the reap.
#[test]
fn a_parent_unknown_in_a_later_reading_keeps_the_one_known_before() {
    let mut meter = Meter::default();
    poll(
        &mut meter,
        &[(10, proc_row(S, 0, 1)), (11, proc_row(S, 0, 10))],
    );
    poll(
        &mut meter,
        &[(10, proc_row(S, 0, 1)), (11, proc_row(2 * S, 0, 0))],
    );
    poll(&mut meter, &[(10, proc_row(S, 0, 1))]);
    reap(&mut meter, 10, 4);
    assert_eq!(user_micros(&meter), 4_000_000);
}

/// Catches: a process below an orphan taken for part of the leader's chain because its
/// parent is a process of the same poll. The orphan (pid 12, parent launchd) and its
/// child (14, 1 s) were alive at the last poll; neither is in the reap (4 s), so both are
/// added: 4 + 2 + 1 = 7 s.
#[test]
fn what_runs_below_an_orphan_is_added_to_the_reap_too() {
    let mut meter = Meter::default();
    poll(
        &mut meter,
        &[
            (10, proc_row(S, 0, 1)),
            (12, proc_row(2 * S, 0, 1)),
            (14, proc_row(S, 0, 12)),
        ],
    );
    reap(&mut meter, 10, 4);
    assert_eq!(user_micros(&meter), 7_000_000);
}

/// Catches: a process whose parent no reading could name (0) taken for an orphan and added
/// on top of the reap, which may hold it. Read once, alive at the last poll, with 3 s; the
/// reap (4 s) is the larger of it and the poll (1 s + 3 s), so the total is 4 s, not 7.
#[test]
fn a_process_with_an_unknown_parent_is_taken_to_be_in_the_chain() {
    let mut meter = Meter::default();
    poll(
        &mut meter,
        &[(10, proc_row(S, 0, 1)), (11, proc_row(3 * S, 0, 0))],
    );
    reap(&mut meter, 10, 4);
    assert_eq!(user_micros(&meter), 4_000_000);
}
