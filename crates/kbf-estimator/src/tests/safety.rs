//! The safety state machine: kill floors, events, decay, raise-only.

use super::*;

fn at(s: &Summaries, k: &Key, day: Day) -> Result<Estimate, Fallback> {
    s.estimate(k, day, &Bounds::default(), &Hint::default())
}

fn kill_on(s: &mut Summaries, k: &Key, run: &Run, day: Day) {
    s.observe(k, run, day, &Bounds::default());
}

/// Rule 3.4.1: a kill sets the key floor to the ladder's next rung, and the next
/// estimate is above the booking it died at.
#[test]
fn a_kill_sets_the_key_floor_above_the_booking() {
    let mut s = Summaries::default();
    let k = linux(1);
    kill_on(&mut s, &k, &kill(4 * GIB, 6 * GIB, Some(16 * GIB)), DAY);
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
    kill_on(&mut s, &k, &kill(4 * GIB, GIB, None), DAY);
    assert_eq!(floor(&s, &k), 4 * GIB + 256 * MIB);
    kill_on(&mut s, &k, &kill(4 * GIB, GIB, Some(GIB)), DAY);
    assert_eq!(floor(&s, &k), 4 * GIB + 256 * MIB);
    assert!(memory(&s, &k) > 4 * GIB);
}

/// A kill is acted on whatever its wall time (the safety effect does not wait for a
/// "trusted" duration).
#[test]
fn a_short_kill_still_sets_the_floor() {
    let mut s = Summaries::default();
    let k = linux(1);
    let mut run = heavy_kill();
    run.wall_us = 0;
    assert_eq!(s.observe(&k, &run, DAY, &Bounds::default()).memory, Ok(()));
    assert_eq!(floor(&s, &k), 16 * GIB);
}

/// Rule 3.4.3: a node-pressure kill never touches the floor or the samples.
#[test]
fn a_node_pressure_kill_is_ignored() {
    let mut s = Summaries::default();
    let k = linux(1);
    let mut run = heavy_kill();
    run.outcome = Outcome::NodePressureKill;
    let seen = s.observe(&k, &run, DAY, &Bounds::default());
    assert_eq!(seen.memory, Err(Discard::NotASample));
    assert_eq!(s.get(&k).unwrap().samples(), 0);
    assert_eq!(floor(&s, &k), 0);
}

/// S1: kills are counted as events. A burst of concurrent operations of one key, all
/// booked before the floor was raised, is one event: the key is not disabled, and
/// every later booking is at least the raised rung.
#[test]
fn a_burst_of_kills_is_one_event() {
    let mut s = Summaries::default();
    let k = linux(1);
    for _ in 0..50 {
        kill_on(&mut s, &k, &heavy_kill(), DAY);
    }
    assert!(!s.get(&k).unwrap().raise_only(DAY), "50 kills, one event");
    assert_eq!(floor(&s, &k), 16 * GIB);
    for _ in 0..50 {
        assert!(memory(&s, &k) >= 16 * GIB);
    }
    // A kill of an attempt booked at the raised rung is a new event, and the second
    // within 256 runs.
    kill_on(&mut s, &k, &kill(16 * GIB, 0, Some(32 * GIB)), DAY);
    assert!(s.get(&k).unwrap().raise_only(DAY));
    assert_eq!(floor(&s, &k), 32 * GIB);
}

/// Rule 3.4.4: a heavy mode that recurs every 30 runs never lets the floor go.
#[test]
fn a_recurring_heavy_mode_never_decays_the_floor() {
    let mut s = Summaries::default();
    let k = linux(1);
    s.observe(&k, &heavy_kill(), DAY, &Bounds::default());
    for i in 1..=3000u32 {
        let peak = if i % 30 == 0 { 9 * GIB } else { 500 * MIB };
        feed_on(&mut s, &k, &success(peak), 1, Day(DAY.0 + i / 20));
        assert_eq!(s.key_floor(&k, &Bounds::default()), 16 * GIB, "run {i}");
    }
}

/// Rule 3.4.4: the floor halves after 256 runs without the heavy mode, once per 256
/// runs, down to nothing below the minimum estimate.
#[test]
fn the_floor_halves_after_256_runs_without_the_heavy_mode() {
    let mut s = Summaries::default();
    let k = linux(1);
    let b = Bounds::default();
    s.observe(&k, &heavy_kill(), DAY, &b);
    // Every run is on day 10 or later, so only the run count can hold the floor.
    let mut day = Day(DAY.0 + 10);
    feed_on(&mut s, &k, &success(500 * MIB), 255, day);
    assert_eq!(s.key_floor(&k, &b), 16 * GIB, "255 runs are not enough");
    feed_on(&mut s, &k, &success(500 * MIB), 1, day);
    assert_eq!(s.key_floor(&k, &b), 8 * GIB, "the 256th run halves it");
    for (now, next) in [(8, 4), (4, 2), (2, 1), (1, 0)] {
        day = Day(day.0 + 8);
        feed_on(&mut s, &k, &success(500 * MIB), 255, day);
        assert_eq!(
            s.key_floor(&k, &b),
            now * GIB,
            "255 runs after the last halving"
        );
        feed_on(&mut s, &k, &success(500 * MIB), 1, day);
        assert_eq!(s.key_floor(&k, &b), next * GIB, "half of {now} GiB");
    }
}

/// S3: a floor also needs 7 days: any number of runs on the day it was set, or in the
/// six days after, leave it. It keeps the floor of a raise-only key the same way.
#[test]
fn the_floor_needs_seven_days_as_well_as_256_runs() {
    let mut s = Summaries::default();
    let k = linux(1);
    let b = Bounds::default();
    s.observe(&k, &heavy_kill(), DAY, &b);
    feed(&mut s, &k, &success(500 * MIB), 600);
    assert_eq!(floor(&s, &k), 16 * GIB, "600 runs on the same day");
    let day6 = Day(DAY.0 + 6);
    feed_on(&mut s, &k, &success(500 * MIB), 1, day6);
    assert_eq!(s.key_floor(&k, &b), 16 * GIB, "six days");
    let day7 = Day(DAY.0 + 7);
    feed_on(&mut s, &k, &success(500 * MIB), 1, day7);
    assert_eq!(s.key_floor(&k, &b), 8 * GIB, "seven days");
}

/// S3: the largest sample of the last 8 days holds the floor even after the 256-run
/// window has forgotten it (a heavy mode rarer than 256 runs but seen this week).
#[test]
fn a_heavy_day_this_week_holds_the_floor() {
    let mut s = Summaries::default();
    let k = linux(1);
    let b = Bounds::default();
    s.observe(&k, &heavy_kill(), DAY, &b);
    feed(&mut s, &k, &success(9 * GIB), 1);
    feed(&mut s, &k, &success(500 * MIB), 300);
    assert!(
        s.get(&k).unwrap().long_max() < 9 * GIB,
        "the run window forgot it"
    );
    let day7 = Day(DAY.0 + 7);
    feed_on(&mut s, &k, &success(500 * MIB), 1, day7);
    assert_eq!(
        s.key_floor(&k, &b),
        16 * GIB,
        "9 GiB, seven days ago, is half of 16 GiB"
    );
    let day9 = Day(DAY.0 + 9);
    feed_on(&mut s, &k, &success(500 * MIB), 1, day9);
    assert_eq!(
        s.key_floor(&k, &b),
        8 * GIB,
        "nine days ago is out of the week"
    );
}

/// S13: time never runs backward inside a summary: a caller's day behind the last
/// sighting is taken as the sighting.
#[test]
fn a_clock_behind_the_last_sighting_is_ignored() {
    let mut s = Summaries::default();
    let k = linux(1);
    let b = Bounds::default();
    s.observe(&k, &heavy_kill(), DAY, &b);
    s.observe(&k, &kill(16 * GIB, 0, Some(32 * GIB)), DAY, &b);
    let summary = s.get(&k).unwrap();
    assert!(summary.raise_only(Day(DAY.0 + 6)));
    assert!(!summary.raise_only(Day(DAY.0 + 7)));
    feed_on(&mut s, &k, &success(GIB), 1, Day(DAY.0 + 10));
    feed_on(&mut s, &k, &success(GIB), 1, Day(DAY.0 - 50));
    let summary = s.get(&k).unwrap();
    assert_eq!(summary.last_seen(), Day(DAY.0 + 10));
    assert!(
        !summary.raise_only(Day(DAY.0 + 5)),
        "5 is behind the sighting at 10"
    );
}

/// Rule 3.4.5: two kill events in 256 runs make the key raise-only for 7 days: it
/// keeps its floor, never lowers below the default, and does not fall back to "no
/// estimate".
#[test]
fn two_kills_in_256_runs_make_the_key_raise_only() {
    let mut s = Summaries::default();
    let k = linux(1);
    kill_on(&mut s, &k, &kill(GIB, 2 * GIB, Some(GIB * 5 / 4)), DAY);
    feed(&mut s, &k, &success(GIB), 100);
    assert_eq!(
        memory(&s, &k),
        2816 * MIB,
        "one kill: lowered below the default"
    );
    kill_on(&mut s, &k, &kill(GIB * 5 / 4, 0, Some(2 * GIB)), DAY);
    assert_eq!(s.get(&k).unwrap().oom_floor(), 2 * GIB);
    assert_eq!(memory(&s, &k), 4 * GIB, "raise-only: not below the default");
    assert_eq!(at(&s, &k, Day(DAY.0 + 6)).unwrap().memory_bytes, 4 * GIB);
    assert_eq!(
        at(&s, &k, Day(DAY.0 + 7)).unwrap().memory_bytes,
        3 * GIB,
        "the seventh day lowers again"
    );
}

/// Rule 3.4.5 boundary: the second kill event must be within 256 runs of the first.
#[test]
fn the_second_kill_must_be_within_256_runs() {
    let b = Bounds::default();
    for (between, raise_only) in [(254, true), (255, false)] {
        let mut s = Summaries::default();
        let k = linux(1);
        s.observe(&k, &heavy_kill(), DAY, &b);
        feed(&mut s, &k, &success(500 * MIB), between);
        s.observe(&k, &kill(16 * GIB, 0, Some(32 * GIB)), DAY, &b);
        assert_eq!(
            s.get(&k).unwrap().raise_only(DAY),
            raise_only,
            "{between} between"
        );
    }
}

/// A kill at the ceiling (the floor already as high as it may go) is still a new event
/// when it was booked at the floor.
#[test]
fn a_kill_at_the_ceiling_is_still_an_event() {
    let b = Bounds {
        host_memory: 8 * GIB,
        ..Bounds::default()
    };
    let mut s = Summaries::default();
    let k = linux(1);
    s.observe(&k, &kill(4 * GIB, 0, Some(16 * GIB)), DAY, &b);
    assert_eq!(s.key_floor(&k, &b), 8 * GIB - 256 * MIB);
    s.observe(&k, &kill(8 * GIB - 256 * MIB, 0, None), DAY, &b);
    assert!(s.get(&k).unwrap().raise_only(DAY));
    assert_eq!(s.key_floor(&k, &b), 8 * GIB - 256 * MIB);
}

/// A kill of an attempt granted before the floor was raised, which asks for more than
/// the floor, still raises the floor (but is not a new event).
#[test]
fn a_late_kill_with_a_higher_rung_raises_the_floor_in_the_same_event() {
    let mut s = Summaries::default();
    let k = linux(1);
    s.observe(&k, &heavy_kill(), DAY, &Bounds::default());
    s.observe(
        &k,
        &kill(4 * GIB, 0, Some(20 * GIB)),
        DAY,
        &Bounds::default(),
    );
    assert_eq!(floor(&s, &k), 20 * GIB);
    assert!(!s.get(&k).unwrap().raise_only(DAY));
}

/// The pure kill rule the scheduler's key floors use: a kill opens an event unless its
/// attempt was booked below the floor in force; the floor takes the retry's rung, never
/// falls, and stays under the host.
#[test]
fn the_kill_floor_rule_counts_events_and_stays_under_the_host() {
    let first = kill_floor(0, 4 * GIB, Some(16 * GIB), 64 * GIB);
    assert_eq!(
        first,
        KillFloor {
            floor: 16 * GIB,
            raised: true,
            new_event: true
        }
    );
    // Booked below the floor in force: the same event, and no higher floor.
    let burst = kill_floor(16 * GIB, 4 * GIB, Some(16 * GIB), 64 * GIB);
    assert_eq!(
        burst,
        KillFloor {
            floor: 16 * GIB,
            raised: false,
            new_event: false
        }
    );
    // Booked at the floor: a new event, one rung up.
    let next = kill_floor(16 * GIB, 16 * GIB, Some(32 * GIB), 64 * GIB);
    assert_eq!(
        (next.floor, next.raised, next.new_event),
        (32 * GIB, true, true)
    );
    // No rung given: one step above the booking.
    assert_eq!(
        kill_floor(0, 4 * GIB, None, 64 * GIB).floor,
        4 * GIB + 256 * MIB
    );
    // The rung is the whole host: the floor stays strictly below it.
    let capped = kill_floor(0, 32 * GIB, Some(64 * GIB), 64 * GIB);
    assert_eq!(capped.floor, 64 * GIB - 256 * MIB);
    assert_eq!(floor_ceiling(64 * GIB), 64 * GIB - 256 * MIB);
    assert_eq!(floor_ceiling(64 * GIB + 1), 64 * GIB);
    // A node with no memory leaves no floor.
    assert_eq!(kill_floor(0, 4 * GIB, Some(16 * GIB), 0).floor, 0);
    assert_eq!(floor_ceiling(0), 0);
}
