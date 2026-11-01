//! `KbfUsageSink` and `RunKeys` under a `js-usage` `UsageHub`, over the in-memory state store.

mod support;

use std::sync::Arc;

use js_usage::{
    Beat, BeatLoad, DropReason, Finish, GRACE_BEATS, KeySource, Limits, UsageHub, UsageSink,
};
use kbf_state::Store;
use kbf_types::sched::usage::{Curve, CurveSource};
use kbf_types::work_key;
use kbf_usage::{KbfUsageSink, MAX_SHAPES, ProfileSink, RunKeys, ShapeIndex};
use support::{Flaky, action, beat, digest, fake, host, lease, minute_of, profile};

const BUDGET: u64 = 1 << 20;
const DAY: u64 = 100;
const MINUTES_PER_DAY: u64 = 1440;
const NOW: u64 = DAY * MINUTES_PER_DAY;
const NO_LIMITS: Limits = Limits {
    threads: 0,
    memory_bytes: 0,
};

/// A hub over the farm's pieces, as the server will build it.
struct Rig {
    keys: Arc<RunKeys>,
    sink: Arc<ProfileSink>,
    seam: Arc<KbfUsageSink>,
    hub: UsageHub,
}

/// A rig over `store` that knows action 1 of instance `main` as profile `k1`.
fn rig(store: Arc<dyn Store>) -> Rig {
    let shapes = Arc::new(ShapeIndex::new());
    shapes.insert(action("main", 1), profile("k1"));
    let sink = Arc::new(ProfileSink::open(store, &shapes, BUDGET, DAY).expect("the store opens"));
    let keys = Arc::new(RunKeys::new(shapes));
    let seam = Arc::new(KbfUsageSink::new(Arc::clone(&sink)));
    let hub = UsageHub::new(seam.clone(), keys.clone(), NO_LIMITS, NOW);
    Rig {
        keys,
        sink,
        seam,
        hub,
    }
}

fn run_id(n: u64) -> String {
    format!("run-1-{n}")
}

fn anyone(_: &js_types::LeaseId, _: &js_types::HostId) -> bool {
    true
}

/// One beat of host `h1` at wall minute `NOW`.
fn send(rig: &Rig, load: BeatLoad, results: &[js_usage::AcceptedResult<'_>]) {
    let id = host("h1");
    let beat = Beat {
        host: &id,
        epoch: 1,
        load,
        results,
    };
    rig.hub.on_beat(beat, NOW, &anyone);
}

/// Run `n` of action 1, three clean minutes, ended with `finish`; its grace beats are
/// sent only when `grace` is true.
fn run(rig: &Rig, n: u64, finish: Finish, bound: bool, grace: bool) {
    let id = run_id(n);
    if bound {
        assert!(rig.keys.bind(&id, action("main", 1)));
    }
    let minutes = (0..3).map(|m| minute_of(lease(n), m, 1_000)).collect();
    let load = BeatLoad::Read {
        load: beat(n, minutes),
        records_dropped: 0,
    };
    let result = js_usage::AcceptedResult {
        lease: lease(n),
        run_id: &id,
        finish,
    };
    send(rig, load, &[result]);
    if grace {
        for _ in 0..GRACE_BEATS {
            send(rig, BeatLoad::Absent, &[]);
        }
    }
}

fn curve_of(rig: &Rig) -> Option<Curve> {
    rig.sink.curves().curve(&work_key("main", digest(1)))
}

// Catches a hub whose runs do not reach the profile: runs_learned moves with each clean run
// handed over after its grace, and a curve is served for the action only from the third.
#[test]
fn three_clean_runs_give_a_curve_and_two_do_not() {
    let rig = rig(fake());
    for n in 1..=2 {
        run(&rig, n, Finish::Clean, true, false);
        assert_eq!(rig.hub.counters().runs_learned, n - 1, "before the grace");
        for _ in 0..GRACE_BEATS {
            send(&rig, BeatLoad::Absent, &[]);
        }
        assert_eq!(rig.hub.counters().runs_learned, n, "after the grace");
        assert_eq!(curve_of(&rig), None, "after {n} runs");
    }
    run(&rig, 3, Finish::Clean, true, true);
    assert_eq!(rig.hub.counters().runs_learned, 3);
    let curve = curve_of(&rig).expect("a curve after three runs");
    assert_eq!(curve.runs(), 3);
    assert_eq!(rig.sink.stats().keys, 1);
}

// Catches a hub that learns from a run that did not end on its own: a killed run files no
// profile row and moves nothing.
#[test]
fn an_unclean_run_teaches_nothing() {
    let store = fake();
    let rig = rig(store.clone());
    run(&rig, 1, Finish::Unclean, true, true);
    assert_eq!(rig.hub.counters().runs_learned, 0);
    assert_eq!(rig.sink.stats().keys, 0);
    assert!(store.get("usage/key/k1").unwrap().is_none());
}

// Catches a run with no key being learned under some other key, or not counted: it is
// counted and files nothing.
#[test]
fn a_run_without_a_key_is_counted_not_learned() {
    let rig = rig(fake());
    run(&rig, 1, Finish::Clean, false, true);
    let counters = rig.hub.counters();
    assert_eq!(counters.runs_without_key, 1);
    assert_eq!(counters.runs_learned, 0);
    assert_eq!(rig.sink.stats().keys, 0);
}

// Catches a run whose action the shape index has forgotten still being keyed: the profile
// name comes from the index at the time the result is read.
#[test]
fn a_run_of_an_action_the_index_forgot_has_no_key() {
    let shapes = Arc::new(ShapeIndex::new());
    let keys = RunKeys::new(Arc::clone(&shapes));
    shapes.insert(action("main", 1), profile("k1"));
    assert!(keys.bind("r1", action("main", 1)));
    assert_eq!(keys.key_of("r1"), Some(profile("k1")));
    assert!(shapes.remove(&action("main", 1)));
    assert_eq!(keys.key_of("r1"), None);
    assert_eq!(keys.key_of("never-bound"), None);
}

// Catches the table's bookkeeping: bind, rebind to another action, forget, and counts.
#[test]
fn bound_runs_are_held_rebound_and_forgotten() {
    let shapes = Arc::new(ShapeIndex::new());
    shapes.insert(action("main", 1), profile("k1"));
    shapes.insert(action("main", 2), profile("k2"));
    let keys = RunKeys::new(shapes);
    assert!(keys.is_empty());
    assert!(keys.bind("r1", action("main", 1)));
    assert!(keys.bind("r1", action("main", 2)));
    assert_eq!(keys.len(), 1);
    assert_eq!(keys.key_of("r1"), Some(profile("k2")));
    assert!(keys.forget("r1"));
    assert!(!keys.forget("r1"));
    assert!(keys.is_empty());
    assert_eq!(keys.key_of("r1"), None);
}

// Catches a table without a bound: a full table refuses a new run, still takes a rebind,
// and has room again after a forget.
#[test]
fn the_table_of_runs_is_bounded() {
    let keys = RunKeys::new(Arc::new(ShapeIndex::new()));
    for n in 0..MAX_SHAPES {
        assert!(keys.bind(&format!("r{n}"), action("main", 1)));
    }
    assert!(!keys.bind("extra", action("main", 1)));
    assert!(keys.bind("r0", action("main", 2)));
    assert_eq!(keys.len(), MAX_SHAPES);
    assert!(keys.forget("r1"));
    assert!(keys.bind("extra", action("main", 1)));
}

// Catches a restart that loses what was learned: a rig over the same store serves the same
// curve, with an empty recorder and nothing counted by the new hub.
#[test]
fn a_restart_keeps_the_curves() {
    let store = fake();
    let first = rig(store.clone());
    for n in 1..=3 {
        run(&first, n, Finish::Clean, true, true);
    }
    let before = curve_of(&first).expect("a curve");
    drop(first);

    let second = rig(store);
    assert_eq!(curve_of(&second), Some(before));
    assert_eq!(second.sink.stats().recorder, js_usage::Counters::default());
    assert_eq!(second.hub.counters().runs_learned, 0);
}

// Catches a seam that gets the day wrong: a run ended at a wall minute of day 100 leaves
// the key last used on day 100, and a flush at a minute of day 200 makes the curves judge
// staleness on day 200 (the key has no curve), with no node rows closed.
#[test]
fn minutes_are_days_and_a_flush_moves_the_curves_day() {
    let rig = rig(fake());
    for n in 1..=3 {
        run(&rig, n, Finish::Clean, true, true);
    }
    let stored = rig
        .seam
        .key_profile(&profile("k1"))
        .unwrap()
        .expect("a profile");
    assert_eq!(stored.runs, 3);
    assert_eq!(stored.last_used_day, DAY);
    assert!(curve_of(&rig).is_some());
    assert_eq!(rig.seam.flush(200 * MINUTES_PER_DAY + 7), Ok(Vec::new()));
    assert_eq!(curve_of(&rig), None);
    assert_eq!(rig.seam.key_profile(&profile("nope")), Ok(None));
}

// Catches a seam that answers for a node series kbf does not keep, or hides the sink's
// curves and counts: prune and node rows are empty successes, the curves are the sink's own,
// and a dropped reading is counted by reason.
#[test]
fn the_seam_has_no_node_series_and_shares_the_curves_and_counts() {
    let rig = rig(fake());
    assert_eq!(rig.seam.prune(NOW), Ok(()));
    let rows = rig
        .seam
        .node_rows(&host("h1"), 0, NOW, std::num::NonZeroUsize::MIN);
    assert_eq!(rows, Ok(Vec::new()));
    let curves = rig.seam.curves().expect("curves");
    assert!(Arc::ptr_eq(&curves, &rig.sink.curves()));
    let seen = rig
        .seam
        .observe(&host("h1"), 1, &beat(1, Vec::new()), NO_LIMITS, 10);
    assert_eq!(
        seen.observed,
        js_usage::Observed::Dropped(DropReason::NoBaseline)
    );
    let counters = rig.seam.counters();
    assert_eq!(counters.dropped.get(&DropReason::NoBaseline), Some(&1));
    assert_eq!(counters.minutes_deduped, 0);
}

// Catches a store failure that is lost or that stops the hub: a learned run that cannot be
// written is counted as a fault and not as learned, and the flush, run and profile reads
// each return the failure.
#[test]
fn a_store_failure_is_a_fault_and_the_hub_goes_on() {
    let store = Flaky::over(fake());
    let rig = rig(store.clone());
    store.set_down(true);
    run(&rig, 1, Finish::Clean, true, true);
    let counters = rig.hub.counters();
    assert_eq!(counters.faults, 1);
    assert_eq!(counters.runs_learned, 0);
    assert!(rig.seam.key_profile(&profile("k1")).is_err());
    store.set_down(false);
    run(&rig, 2, Finish::Clean, true, true);
    assert_eq!(rig.hub.counters().runs_learned, 1);
    for n in 3..=4 {
        run(&rig, n, Finish::Clean, true, true);
    }
    rig.sink.touch(&profile("k1"), DAY + 1);
    store.set_down(true);
    let err = rig.seam.flush(NOW).unwrap_err();
    assert!(err.0.starts_with("state store"), "{}", err.0);
}

// Catches a run the profile declines being counted as a run in a profile: the hub counts it
// as learned (the call succeeded), and the seam counts it apart, so the two numbers say how
// many runs are in profiles. The run has minutes 0 and 9 only, more than a fifth missing.
#[test]
fn a_run_the_profile_declines_is_counted_apart() {
    let rig = rig(fake());
    let id = run_id(1);
    assert!(rig.keys.bind(&id, action("main", 1)));
    let minutes = [0, 9].map(|m| minute_of(lease(1), m, 1_000)).to_vec();
    let load = BeatLoad::Read {
        load: beat(1, minutes),
        records_dropped: 0,
    };
    let result = js_usage::AcceptedResult {
        lease: lease(1),
        run_id: &id,
        finish: Finish::Clean,
    };
    send(&rig, load, &[result]);
    for _ in 0..GRACE_BEATS {
        send(&rig, BeatLoad::Absent, &[]);
    }
    assert_eq!(rig.hub.counters().runs_learned, 1);
    assert_eq!(rig.seam.declined(), 1);
    assert_eq!(rig.sink.stats().keys, 0);
    run(&rig, 2, Finish::Clean, true, true);
    assert_eq!(rig.hub.counters().runs_learned, 2);
    assert_eq!(rig.seam.declined(), 1);
    assert_eq!(rig.sink.stats().keys, 1);
}

// Catches a full table of runs that refuses without a trace: each refused bind is counted,
// a rebind of a held run is not a refusal, and a bind that fits is not counted.
#[test]
fn a_refused_bind_is_counted() {
    let keys = RunKeys::new(Arc::new(ShapeIndex::new()));
    for n in 0..MAX_SHAPES {
        assert!(keys.bind(&format!("r{n}"), action("main", 1)));
    }
    assert_eq!(keys.refused(), 0);
    assert!(!keys.bind("extra", action("main", 1)));
    assert!(!keys.bind("extra2", action("main", 1)));
    assert!(keys.bind("r0", action("main", 2)));
    assert_eq!(keys.refused(), 2);
}
