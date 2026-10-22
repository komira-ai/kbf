//! The replay loop against a real scheduler: what each arm books, what is learned and
//! when, and how queue wait, backlog and the funnel are counted.

mod common;

use common::*;
use kbf_estimator::{Fallback, Level};
use kbf_estimator_replay::replay::sim::{ArmRun, Lookup, SimError, raised_by, simulate};
use kbf_estimator_replay::replay::stats::{Waits, Window, section};
use kbf_estimator_replay::replay::trace::Trace;
use kbf_estimator_replay::replay::{Arm, Params};

fn run(trace: &Trace, arm: Arm) -> ArmRun {
    simulate(trace, arm, &Params::default()).unwrap()
}

fn kills(run: &ArmRun) -> usize {
    run.attempts.iter().filter(|a| a.killed).count()
}

fn big() -> [(&'static str, u64, u64); 1] {
    [("n1", 64, 64)]
}

/// Catches look-ahead: an estimator that hears of an attempt when it starts, not when it
/// ends. Twenty operations of one key arrive a second apart and each runs 100 s before it
/// is killed, so nothing is known when any of them is booked: the floor arm must kill as
/// often as the null arm. An operation that arrives after the first kill ended books the
/// raised rung and is not killed.
#[test]
fn an_arrival_learns_only_from_attempts_that_ended() {
    let mut ops: Vec<_> = (0..20)
        .map(|i| op("main", 1, i, 10 * GIB, 100, 100))
        .collect();
    ops.extend((0..5).map(|i| op("main", 1, 500 + i, 10 * GIB, 100, 100)));
    let t = trace(ops, &big());
    let null = run(&t, Arm::Null);
    let floor = run(&t, Arm::KeyFloor);
    assert_eq!(kills(&null), 25, "every operation is killed once at 4 GiB");
    assert_eq!(
        kills(&floor),
        20,
        "the 20 in flight together learned nothing"
    );
    for rec in &floor.ops[..20] {
        assert_eq!(rec.submitted_memory, 4 * GIB);
    }
    for rec in &floor.ops[20..] {
        assert_eq!(rec.submitted_memory, 16 * GIB, "the floor is the next rung");
        assert!(rec.raised);
    }
}

/// Catches a floor keyed by the shape alone: a kill in one instance must not raise the
/// booking of another instance's action of the same shape, and must raise the first's own.
#[test]
fn a_floor_belongs_to_the_instance_that_was_killed() {
    let ops = vec![
        op("a", 7, 0, 10 * GIB, 100, 100),
        op("b", 7, 1_000, 2 * GIB, 10, 10),
        op("a", 7, 2_000, 10 * GIB, 100, 100),
    ];
    let floor = run(&trace(ops, &big()), Arm::KeyFloor);
    assert_eq!(
        floor.ops[1].submitted_memory,
        4 * GIB,
        "instance b is not raised"
    );
    assert_eq!(floor.ops[2].submitted_memory, 16 * GIB, "instance a is");
    assert_eq!(kills(&floor), 1);
}

/// Catches a floor that reaches the host: on a 16 GiB node the next rung after 4 GiB is
/// the whole host, which runs alone. The floor stays one 256 MiB step below it.
#[test]
fn the_floor_stays_below_the_host() {
    let ops = vec![
        op("main", 1, 0, 12 * GIB, 100, 100),
        op("main", 1, 1_000, 12 * GIB, 100, 100),
    ];
    let floor = run(&trace(ops, &[("n1", 8, 16)]), Arm::KeyFloor);
    assert_eq!(floor.host_memory, 16 * GIB);
    assert_eq!(floor.ops[1].submitted_memory, 16 * GIB - 256 * MIB);
    assert!(floor.ops[1].submitted_memory < floor.host_memory);
    assert_eq!(
        kills(&floor),
        1,
        "the first climbed to the host; the second did not need to"
    );
}

/// Catches a hint raised by the floor: an operation whose first booking is not the default
/// rung named its memory, and keeps it.
#[test]
fn a_named_booking_is_not_floored() {
    let mut named = op("main", 1, 1_000, 5 * GIB, 10, 10);
    named.first_booked = 8 * GIB;
    let ops = vec![op("main", 1, 0, 10 * GIB, 100, 100), named];
    let floor = run(&trace(ops, &big()), Arm::KeyFloor);
    assert_eq!(floor.ops[1].submitted_memory, 8 * GIB);
    assert!(!floor.ops[1].raised);
}

/// Catches the upper-bound arm not lowering (it would be the floor arm) and the floor arm
/// lowering (it must never book below the record's first booking).
#[test]
fn only_the_upper_bound_arm_books_below_the_default() {
    let ops: Vec<_> = (0..8)
        .map(|i| op("main", 1, i * 100, GIB, 10, 10))
        .collect();
    let t = trace(ops, &big());
    let upper = run(&t, Arm::SampleUpperBound);
    let floor = run(&t, Arm::KeyFloor);
    assert_eq!(upper.ops[0].submitted_memory, 4 * GIB, "no samples yet");
    let last = upper.ops[7].submitted_memory;
    assert!((GIB..4 * GIB).contains(&last), "lowered to {last}");
    assert!(floor.ops.iter().all(|o| o.submitted_memory == 4 * GIB));
    assert_eq!(kills(&upper), 0, "a 1 GiB need fits any of these limits");
}

/// Catches resolved-level counts that do not follow the samples: an operation with fewer
/// than two samples is cold, then L2 (no configuration) or L1 (one), and the mixed share
/// counts a key whose recent samples are far apart.
#[test]
fn the_shares_follow_what_was_learned() {
    let mut ops: Vec<_> = (0..8)
        .map(|i| op("main", 1, i * 100, GIB, 10, 10))
        .collect();
    // Same shape, with a configuration: L1 once it has samples of its own.
    ops.extend((0..8).map(|i| {
        let mut o = op("main", 2, 2_000 + i * 100, GIB, 10, 10);
        o.key = Some(key("main", 2, Some(9)));
        o
    }));
    let floor = run(&trace(ops, &big()), Arm::KeyFloor);
    let s = section(&floor, Window { first: 1, last: 1 });
    assert_eq!((s.ops, s.l1, s.l2), (16, 6, 6));
    assert_eq!(s.cold.get("Missing"), Some(&2));
    assert_eq!(s.cold.get("Cold"), Some(&2));
    assert!(matches!(
        floor.ops[0].lookup,
        Lookup::Cold(Fallback::Missing)
    ));
    assert!(matches!(floor.ops[1].lookup, Lookup::Cold(Fallback::Cold)));
    assert!(matches!(floor.ops[2].lookup, Lookup::Level(Level::L2)));
    assert!(matches!(floor.ops[10].lookup, Lookup::Level(Level::L1)));

    let mut ops: Vec<_> = (0..5)
        .map(|i| op("main", 1, i * 100, GIB, 10, 10))
        .collect();
    ops.push(op("main", 1, 600, 10 * GIB, 10, 10));
    ops.push(op("main", 1, 1_600, GIB, 10, 10));
    let mixed = run(&trace(ops, &big()), Arm::KeyFloor);
    assert!(!mixed.ops[5].mixed, "five alike samples");
    assert!(mixed.ops[6].mixed, "a 10 GiB sample beside four of 1 GiB");
    let s = section(&mixed, Window { first: 1, last: 1 });
    assert_eq!(s.mixed, 1);
}

/// Catches queue wait measured from the wrong instant, and a scheduler that is not really
/// in the loop: on an 8-core node ten one-core operations arrive together and run 100 s;
/// eight start at once and two wait for a core.
#[test]
fn queue_wait_is_what_the_scheduler_makes_it() {
    let ops: Vec<_> = (0..10).map(|i| op("main", i, 0, GIB, 100, 100)).collect();
    let r = run(&trace(ops, &[("n1", 8, 64)]), Arm::Null);
    let s = section(&r, Window { first: 1, last: 1 });
    assert_eq!(
        s.waits,
        Some(Waits {
            p50: 0,
            p95: 100_000,
            p99: 100_000
        })
    );
    assert_eq!(s.kills.of, 10);
    // Done after 200 s, long before the day ends.
    assert_eq!(s.drain_ms, [(1, 0)]);
}

/// Catches a backlog that is not measured from the end of the day: an operation that is
/// still running 600 s after day 1 ends leaves a backlog that drains in 600 s, and a
/// queued attempt's wait counts its rerun (queued again when it was killed), not its
/// arrival.
#[test]
fn a_day_ends_with_the_backlog_it_leaves() {
    let ops = vec![
        op("main", 1, DAY_S - 400, GIB, 1_000, 1_000),
        op("main", 2, DAY_S + 5_000, 10 * GIB, 50, 50),
    ];
    let r = run(&trace(ops, &big()), Arm::Null);
    let s = section(&r, Window { first: 1, last: 2 });
    assert_eq!(s.drain_ms, [(1, 600_000), (2, 0)]);
    // The second operation: killed at 4 GiB after 50 s, queued again, started at once.
    let waits: Vec<u64> = r
        .attempts
        .iter()
        .map(|a| a.started_ms - a.queued_ms)
        .collect();
    assert_eq!(waits, [0, 0, 0]);
    assert_eq!(r.attempts[2].queued_ms, (DAY_S + 5_050) * 1_000);
}

/// Catches an operation nothing can run being left out of the books: a 64 GiB ask on a
/// 16 GiB node is refused after the scheduler's unservable wait and counted as done.
#[test]
fn an_unservable_operation_is_refused_and_counted() {
    let mut huge = op("main", 1, 0, GIB, 10, 10);
    huge.first_booked = 64 * GIB;
    let r = run(&trace(vec![huge], &[("n1", 8, 16)]), Arm::Null);
    assert!(r.ops[0].refused);
    assert!(r.ops[0].finished_ms.is_some());
    assert!(r.attempts.is_empty());
    let s = section(&r, Window { first: 1, last: 1 });
    assert_eq!((s.refused, s.ops, s.kills.of), (1, 1, 0));
}

/// Catches two submissions of one action in flight counted twice: the second joins the
/// first, ends with it, and is reported as joined.
#[test]
fn an_action_in_flight_is_joined_not_run_twice() {
    let mut twin = op("main", 1, 5, GIB, 10, 10);
    twin.digest = "d-main-1-0".to_owned();
    let ops = vec![op("main", 1, 0, GIB, 100, 100), twin];
    let r = run(&trace(ops, &big()), Arm::Null);
    assert!(r.ops[1].joined);
    assert_eq!(r.attempts.len(), 1);
    assert_eq!(r.ops[0].finished_ms, r.ops[1].finished_ms);
    let s = section(&r, Window { first: 1, last: 1 });
    assert_eq!((s.ops, s.joined), (1, 1));
}

/// Catches samples counted that the estimator refused: a run under a second is not a
/// memory sample, and an action with no shape key is never shown to it.
#[test]
fn the_funnel_counts_what_the_estimator_took_and_refused() {
    let mut keyless = op("main", 3, 200, GIB, 10, 10);
    keyless.key = None;
    let ops = vec![
        op("main", 1, 0, GIB, 10, 10),
        op("main", 2, 100, GIB, 0, 0),
        keyless,
    ];
    let r = run(&trace(ops, &big()), Arm::KeyFloor);
    assert_eq!(r.samples.taken, 1);
    assert_eq!(r.samples.refused.get("Untrusted"), Some(&1));
    assert_eq!(r.samples.no_key, 1);
    assert_eq!(r.samples.cpu, 0);
    let s = section(&r, Window { first: 1, last: 1 });
    assert_eq!(s.cold.get("NoKey"), Some(&1));
}

/// Catches a success on a need above the whole host counted as a sample: it completes at
/// the host booking, and the estimator refuses a peak at or above the host.
#[test]
fn a_success_beyond_the_host_is_not_a_sample() {
    let ops = vec![op("main", 1, 0, 70 * GIB, 10, 10)];
    let r = run(&trace(ops, &[("n1", 8, 64)]), Arm::KeyFloor);
    assert_eq!(r.samples.refused.get("OverCap"), Some(&1));
    assert_eq!(
        kills(&r),
        3,
        "4, 16 and 32 GiB are killed; the host runs it"
    );
    assert_eq!(r.attempts.last().unwrap().booked, 64 * GIB);
}

/// Catches a replay that cannot stop: with a budget of two events it gives up and says so.
#[test]
fn a_replay_that_does_not_end_gives_up() {
    let t = trace(vec![op("main", 1, 0, GIB, 10, 10)], &big());
    let params = Params {
        max_events: 2,
        ..Params::default()
    };
    let err = simulate(&t, Arm::Null, &params).unwrap_err();
    assert_eq!(err, SimError::Stuck(2));
    assert!(err.to_string().contains("within 2 events"));
}

/// Catches the requeue reason read wrongly: only an own-limit kill carries a booking.
#[test]
fn only_a_memory_kill_names_a_raised_booking() {
    use kbf_sched::RequeueReason;
    let kill = RequeueReason::OutOfMemory {
        booked: 4 * GIB,
        raised: 16 * GIB,
    };
    assert_eq!(raised_by(kill), Some(16 * GIB));
    assert_eq!(raised_by(RequeueReason::Silent), None);
    assert_eq!(raised_by(RequeueReason::NodeMemoryPressure), None);
}

/// Catches a trace with no node running, and a ladder the scheduler would refuse.
#[test]
fn a_replay_needs_nodes_and_a_ladder() {
    let t = trace(vec![op("main", 1, 0, GIB, 1, 1)], &[]);
    assert_eq!(
        simulate(&t, Arm::Null, &Params::default()).unwrap_err(),
        SimError::NoNodes
    );
    let t = trace(vec![op("main", 1, 0, GIB, 1, 1)], &big());
    let params = Params {
        ladder_gib: vec![8, 4],
        ..Params::default()
    };
    let err = simulate(&t, Arm::Null, &params).unwrap_err();
    assert!(matches!(err, SimError::Ladder(_)), "{err:?}");
    assert!(err.to_string().starts_with("memory ladder"));
}

/// The scheduler keeps its own per-digest floor: a second submission of a digest that was
/// killed starts at the rung the first reached, in every arm (it is not the estimator's).
#[test]
fn the_digest_floor_is_the_schedulers_in_every_arm() {
    let mut again = op("main", 1, 1_000, 10 * GIB, 100, 100);
    again.digest = "d-main-1-0".to_owned();
    let ops = vec![op("main", 1, 0, 10 * GIB, 100, 100), again];
    let r = run(&trace(ops, &big()), Arm::Null);
    let booked: Vec<u64> = r.attempts.iter().map(|a| a.booked / GIB).collect();
    assert_eq!(
        booked,
        [4, 16, 16],
        "the second submission starts at 16 GiB"
    );
}

/// Catches an arrival that cannot see an attempt which ended at the very instant it
/// arrived: the design's rule is `ended <= queued`, so the end is learned first.
#[test]
fn an_arrival_at_the_instant_an_attempt_ends_learns_it() {
    let ops = vec![
        op("main", 1, 0, 10 * GIB, 100, 100),
        op("main", 1, 100, 10 * GIB, 100, 100),
        op("main", 1, 99, 10 * GIB, 100, 100),
    ];
    let floor = run(&trace(ops, &big()), Arm::KeyFloor);
    assert_eq!(
        floor.ops[1].submitted_memory,
        16 * GIB,
        "ended at 100 s, arrived at 100 s"
    );
    assert_eq!(
        floor.ops[2].submitted_memory,
        4 * GIB,
        "one second before it ended"
    );
}
