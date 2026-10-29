//! `ProfileSink` over the in-memory state store.

mod support;

use std::sync::Arc;

use js_usage::{Admitted, DropReason, Limits, Observed, ProfileError};
use kbf_state::Store;
use kbf_types::sched::usage::{CurveSource, MinutePoint};
use kbf_types::work_key;
use kbf_usage::{Flushed, ProfileSink, ShapeIndex};
use support::{Flaky, action, beat, digest, fake, host, lease, minute_of, profile, put, run_of};

const BUDGET: u64 = 1 << 20;
const NO_LIMITS: Limits = Limits {
    threads: 0,
    memory_bytes: 0,
};

/// A sink over `store` that knows action 1 of instance `main` as profile `key`.
fn open(store: Arc<dyn Store>, key: &str, budget: u64, today: u64) -> ProfileSink {
    let shapes = Arc::new(ShapeIndex::new());
    shapes.insert(action("main", 1), profile(key));
    ProfileSink::open(store, &shapes, budget, today).expect("the store opens")
}

fn curve_of(sink: &ProfileSink, tag: u8) -> Option<kbf_types::sched::usage::Curve> {
    sink.curves().curve(&work_key("main", digest(tag)))
}

fn row_name(key: &str) -> String {
    format!("usage/key/{key}")
}

// Catches a sink whose runs never reach the profile of the key they were filed under: three
// runs of one key make a curve, the curve is served for the action the shape index maps to
// that key and for no other, and it holds the runs' own figures.
#[test]
fn three_runs_of_a_key_make_a_curve_for_its_action_only() {
    let sink = open(fake(), "k1", BUDGET, 100);
    for n in 1..=3 {
        assert_eq!(curve_of(&sink, 1), None, "after {} runs", n - 1);
        let admitted = sink.run_ended(&profile("k1"), &run_of(3), 100).unwrap();
        assert_eq!(admitted, Admitted::Yes);
    }
    let curve = curve_of(&sink, 1).expect("a curve after three runs");
    assert_eq!(curve.runs(), 3);
    assert_eq!(
        curve.at(0),
        Some(MinutePoint {
            cores_milli: 1_000,
            working_set_bytes: 5_000,
        })
    );
    assert_eq!(
        curve_of(&sink, 2),
        None,
        "an action the index does not know"
    );
    assert_eq!(sink.stats().keys, 1);
}

// Catches a sink that does not keep what it learned: a second sink over the same store
// serves the same curve, holds no recorder state (a node's first reading is a baseline
// again), and counts no corrupt row.
#[test]
fn a_new_sink_over_the_same_store_restores_the_curves() {
    let store = fake();
    let first = open(store.clone(), "k1", BUDGET, 100);
    for _ in 0..3 {
        first.run_ended(&profile("k1"), &run_of(3), 100).unwrap();
    }
    first.observe(&host("h1"), 1, &beat(1, Vec::new()), NO_LIMITS, 10);
    let before = curve_of(&first, 1).expect("a curve");
    drop(first);

    let second = open(store, "k1", BUDGET, 100);
    assert_eq!(curve_of(&second, 1), Some(before));
    let stats = second.stats();
    assert_eq!((stats.keys, stats.corrupt_rows), (1, 0));
    assert_eq!(stats.recorder.dropped_total(), 0);
    let (observed, _) = second.observe(&host("h1"), 1, &beat(2, Vec::new()), NO_LIMITS, 11);
    assert_eq!(observed, Observed::Dropped(DropReason::NoBaseline));
}

// Catches a run that is stored although the profile refuses it: an empty run and a run
// missing more than a fifth of its minutes answer why, and write no row.
#[test]
fn a_run_the_profile_does_not_take_is_answered_and_not_stored() {
    let store = fake();
    let sink = open(store.clone(), "k1", BUDGET, 100);
    let empty = sink.run_ended(&profile("k1"), &run_of(0), 100).unwrap();
    assert_eq!(empty, Admitted::Empty);
    let mut gappy = run_of(10);
    gappy.minutes.truncate(4);
    gappy.minutes.push(js_usage::RunMinute {
        minute: 9,
        cores_milli: 1,
        ws_bytes: 1,
    });
    let gappy = sink.run_ended(&profile("k1"), &gappy, 100).unwrap();
    assert_eq!(gappy, Admitted::MissingMinutes);
    assert!(store.get(&row_name("k1")).unwrap().is_none());
    assert_eq!(sink.stats().keys, 0);
}

// Catches a recorder that is not reached through the sink: the first reading of a node is
// a baseline that still hands back its lease minutes; a later one is accepted; a minute
// sent twice is handed back once.
#[test]
fn observe_judges_readings_and_hands_each_lease_minute_back_once() {
    let sink = open(fake(), "k1", BUDGET, 100);
    let minute = minute_of(lease(1), 0, 1_500);
    let (first, minutes) = sink.observe(
        &host("h1"),
        1,
        &beat(1, vec![minute.clone()]),
        NO_LIMITS,
        10,
    );
    assert_eq!(first, Observed::Dropped(DropReason::NoBaseline));
    assert_eq!(minutes, vec![minute.clone()]);
    let (second, minutes) = sink.observe(
        &host("h1"),
        1,
        &beat(2, vec![minute.clone()]),
        NO_LIMITS,
        10,
    );
    assert_eq!(second, Observed::Accepted);
    assert!(minutes.is_empty(), "a resent minute is not new");
    assert_eq!(sink.stats().recorder.minutes_deduped, 1);
}

// Catches a node series that grows: 1000 readings over 100 wall minutes leave 101
// accumulated minutes, a flush drops those finished before its minute (not the minute
// itself), counts them, and a second flush finds nothing.
#[test]
fn flush_drops_the_finished_node_minutes() {
    let sink = open(fake(), "k1", BUDGET, 100);
    let h = host("h1");
    for n in 1..=1000 {
        let (_, _) = sink.observe(&h, 1, &beat(n, Vec::new()), NO_LIMITS, n / 10);
    }
    let partial = sink.flush(50, 100).unwrap();
    assert_eq!(
        partial,
        Flushed {
            node_minutes_discarded: 50,
            touches_persisted: 0,
        }
    );
    let rest = sink.flush(101, 100).unwrap();
    assert_eq!(rest.node_minutes_discarded, 51);
    assert_eq!(sink.flush(101, 100).unwrap().node_minutes_discarded, 0);
    assert_eq!(sink.stats().node_minutes_discarded, 101);
}

// Catches a forget that leaves a node's open minutes or its baseline behind.
#[test]
fn forgetting_a_host_drops_its_open_minutes_and_baseline() {
    let sink = open(fake(), "k1", BUDGET, 100);
    let h = host("h1");
    for n in 1..=3 {
        sink.observe(&h, 1, &beat(n, Vec::new()), NO_LIMITS, 5);
    }
    sink.forget_host(&h);
    assert_eq!(sink.flush(10, 100).unwrap().node_minutes_discarded, 0);
    let (observed, _) = sink.observe(&h, 1, &beat(4, Vec::new()), NO_LIMITS, 5);
    assert_eq!(observed, Observed::Dropped(DropReason::NoBaseline));
}

// Catches a day that never reaches the curves (idle 61 days still served) and a touch that
// does not renew: a key idle for more than 60 days has no curve until it is used again,
// and the use is written once.
#[test]
fn a_key_idle_for_61_days_has_no_curve_until_touched() {
    let sink = open(fake(), "k1", BUDGET, 100);
    for _ in 0..3 {
        sink.run_ended(&profile("k1"), &run_of(3), 100).unwrap();
    }
    sink.flush(0, 160).unwrap();
    assert!(curve_of(&sink, 1).is_some(), "idle 60 days");
    sink.flush(0, 161).unwrap();
    assert_eq!(curve_of(&sink, 1), None, "idle 61 days");
    sink.touch(&profile("k1"), 161);
    assert!(curve_of(&sink, 1).is_some());
    let flushed = sink.flush(0, 161).unwrap();
    assert_eq!(flushed.touches_persisted, 1);
    assert_eq!(sink.flush(0, 161).unwrap().touches_persisted, 0);
}

// Catches a touch of a key the store does not hold that makes a row.
#[test]
fn touching_an_unknown_key_writes_nothing() {
    let store = fake();
    let sink = open(store.clone(), "k1", BUDGET, 100);
    sink.touch(&profile("k1"), 200);
    assert_eq!(sink.flush(0, 200).unwrap().touches_persisted, 0);
    assert!(store.get(&row_name("k1")).unwrap().is_none());
}

// Catches a budget that is not kept and one that evicts the wrong key: with room for two
// rows, a third key's run deletes the least recently used one's row (and its curve), and
// a key used since is kept in its place.
#[test]
fn the_budget_evicts_the_least_recently_used_key() {
    let probe = open(fake(), "a", BUDGET, 100);
    probe.run_ended(&profile("a"), &run_of(3), 100).unwrap();
    let one_row = probe.stats().bytes;
    let store = fake();
    let sink = open(store.clone(), "a", one_row * 5 / 2, 100);
    sink.run_ended(&profile("a"), &run_of(3), 100).unwrap();
    sink.run_ended(&profile("b"), &run_of(3), 100).unwrap();
    sink.touch(&profile("a"), 101);
    sink.run_ended(&profile("c"), &run_of(3), 101).unwrap();
    assert!(store.get(&row_name("a")).unwrap().is_some());
    assert!(
        store.get(&row_name("b")).unwrap().is_none(),
        "b was the oldest"
    );
    assert!(store.get(&row_name("c")).unwrap().is_some());
    let stats = sink.stats();
    assert_eq!(stats.keys, 2);
    assert!(stats.bytes <= one_row * 5 / 2);
}

// Catches a corrupt profile row that stops the store from opening, one that is not
// counted, and one that the next run of its key does not replace.
#[test]
fn a_corrupt_row_is_counted_and_replaced_by_the_next_run() {
    let store = fake();
    put(store.as_ref(), &row_name("k1"), b"not a profile row");
    put(store.as_ref(), &row_name("Not-A-Key"), b"x");
    let sink = open(store.clone(), "k1", BUDGET, 100);
    let stats = sink.stats();
    assert_eq!((stats.keys, stats.corrupt_rows), (0, 2));
    sink.run_ended(&profile("k1"), &run_of(3), 100).unwrap();
    assert_eq!(sink.stats().keys, 1);
    let held = store.get(&row_name("k1")).unwrap().unwrap();
    assert_ne!(held.value, b"not a profile row");
    for _ in 0..2 {
        sink.run_ended(&profile("k1"), &run_of(3), 100).unwrap();
    }
    assert!(curve_of(&sink, 1).is_some());
}

// Catches store failures that are swallowed: opening, a run and a day's write each
// return the store's error, and the node minutes a failed flush dropped are still counted.
#[test]
fn store_failures_are_returned() {
    let flaky = Flaky::over(fake());
    flaky.set_down(true);
    let shapes = Arc::new(ShapeIndex::new());
    let failed = ProfileSink::open(flaky.clone(), &shapes, BUDGET, 100);
    assert!(matches!(failed, Err(ProfileError::Store(_))));

    flaky.set_down(false);
    let sink = ProfileSink::open(flaky.clone(), &shapes, BUDGET, 100).unwrap();
    for _ in 0..3 {
        sink.run_ended(&profile("k1"), &run_of(3), 100).unwrap();
    }
    let h = host("h1");
    for n in 1..=3 {
        sink.observe(&h, 1, &beat(n, Vec::new()), NO_LIMITS, 5);
    }
    sink.touch(&profile("k1"), 101);
    flaky.set_down(true);
    assert!(matches!(
        sink.run_ended(&profile("k1"), &run_of(3), 101),
        Err(ProfileError::Store(_))
    ));
    assert!(matches!(sink.flush(10, 101), Err(ProfileError::Store(_))));
    assert_eq!(sink.stats().node_minutes_discarded, 1);
    flaky.set_down(false);
    assert_eq!(sink.flush(10, 101).unwrap().touches_persisted, 1);
}

// Catches a curve handle that is not the one the store fills: the sink hands out one set of
// curves, and a run ended afterwards is visible through a handle taken before it.
#[test]
fn the_curves_handle_is_the_one_the_sink_fills() {
    let sink = open(fake(), "k1", BUDGET, 100);
    let early = sink.curves();
    assert!(Arc::ptr_eq(&early, &sink.curves()));
    for _ in 0..3 {
        sink.run_ended(&profile("k1"), &run_of(3), 100).unwrap();
    }
    assert!(early.curve(&work_key("main", digest(1))).is_some());
}
