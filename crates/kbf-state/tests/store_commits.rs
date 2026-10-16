//! The store-commits gate: the SQLite store must carry the target write rate
//! ([`kbf_state::TARGET_WRITES_PER_SECOND`], 1500 durable writes a second) on far fewer
//! commits than writes. The bound is a count of commits, which does not depend on how fast
//! the machine syncs; the wall-clock side of the bound is the commit-latency metric
//! ([`kbf_state::sqlite::CommitMetrics`]) the operator watches, held by the second test.

mod common;

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use common::{open_manual, scratch};
use kbf_state::sqlite::Options;
use kbf_state::{Cond, GROUP_COMMIT_WINDOW, Store, TARGET_WRITES_PER_SECOND, Txn};

const WRITERS: usize = 50;

// Catches a writer that syncs per write, or one that does not group concurrent writers:
// 1500 writes from 50 callers, each waiting for its acknowledgement before the next, must
// take exactly one commit per round of 50. The window is far longer than the test needs, so
// a group closes only because it is full, never because time ran out.
#[test]
fn the_target_rate_takes_one_commit_per_group() {
    let writes = usize::try_from(TARGET_WRITES_PER_SECOND).expect("the rate fits a usize");
    let rounds = writes / WRITERS;
    assert_eq!(
        rounds * WRITERS,
        writes,
        "the writers divide the rate evenly"
    );

    let dir = scratch("store-commits");
    let options = Options {
        window: Duration::from_secs(30),
        max_group: WRITERS,
        ..Options::default()
    };
    let (store, _clock) = open_manual(&dir, options);
    let store = Arc::new(store);
    let before = store.commit_stats();

    // The callers and this thread meet after every round, so a group that did not close
    // when it filled is caught at the end of its round, not by a test that never finishes.
    let barrier = Arc::new(Barrier::new(WRITERS + 1));
    let callers: Vec<_> = (0..WRITERS)
        .map(|writer| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                for round in 0..rounds {
                    let key = format!("gate/{writer:02}/{round:03}");
                    store
                        .txn(Txn::new().put(key, "x", Cond::Any))
                        .expect("the store is reachable")
                        .expect("an unconditional put commits");
                    // Once everyone has its acknowledgement, and again once the checks
                    // of the round are made: no write of the next round is in flight
                    // while they are.
                    barrier.wait();
                    barrier.wait();
                }
            })
        })
        .collect();
    for round in 0..rounds {
        barrier.wait();
        let metrics = store.commit_metrics();
        assert_eq!(
            metrics.groups,
            round as u64 + 1,
            "round {round}: every round of {WRITERS} callers is one group"
        );
        assert!(
            metrics.max_latency < options.window,
            "round {round}: a group waited {:?}, so it closed on the {:?} window and not because it was full (max_group ignored?)",
            metrics.max_latency,
            options.window
        );
        barrier.wait();
    }
    for caller in callers {
        caller.join().expect("a caller finished");
    }

    let stats = store.commit_stats();
    assert_eq!(
        stats.txns - before.txns,
        writes as u64,
        "every write committed"
    );
    let commits = stats.syncs - before.syncs;
    assert_eq!(
        commits, rounds as u64,
        "{writes} writes took {commits} commits; one per round of {WRITERS} is the bound"
    );
    assert!(
        commits * 10 <= writes as u64,
        "grouping must cut commits by at least ten times"
    );
    let metrics = store.commit_metrics();
    assert_eq!(metrics.groups, rounds as u64);
    assert_eq!(metrics.largest_group, WRITERS as u64);
    assert_eq!(
        store.scan_prefix("gate/").expect("scan").len(),
        writes,
        "every acknowledged write is readable"
    );
}

// Catches a writer that holds a group longer than the commit window allows, measured by the
// store's own latency metric under the target write rate: 1500 writes from 50 callers on the
// real window. The bound is the window with a generous allowance for the sync and for a busy
// runner, so it fails on a writer that waits several windows, not on a slow disk; the exact
// counts are held by the test above, which this one does not repeat.
#[test]
fn the_target_rate_stays_inside_the_commit_window() {
    let writes = usize::try_from(TARGET_WRITES_PER_SECOND).expect("the rate fits a usize");
    let rounds = writes / WRITERS;
    let bound = GROUP_COMMIT_WINDOW * 100;

    let dir = scratch("store-latency");
    let options = Options {
        latency_limit: bound,
        ..Options::default()
    };
    assert_eq!(options.window, GROUP_COMMIT_WINDOW, "the real window");
    let (store, _clock) = open_manual(&dir, options);
    let store = Arc::new(store);

    let callers: Vec<_> = (0..WRITERS)
        .map(|writer| {
            let store = Arc::clone(&store);
            thread::spawn(move || {
                for round in 0..rounds {
                    let key = format!("latency/{writer:02}/{round:03}");
                    store
                        .txn(Txn::new().put(key, "x", Cond::Any))
                        .expect("the store is reachable")
                        .expect("an unconditional put commits");
                }
            })
        })
        .collect();
    for caller in callers {
        caller.join().expect("a caller finished");
    }

    let metrics = store.commit_metrics();
    assert_eq!(
        store.commit_stats().txns,
        writes as u64,
        "every write committed"
    );
    assert!(
        metrics.groups * 2 <= writes as u64,
        "{writes} writes took {} groups; grouping must at least halve them",
        metrics.groups
    );
    assert_eq!(
        metrics.over_limit, 0,
        "{} of {} groups took longer than {bound:?} (the longest took {:?}); the commit window is {GROUP_COMMIT_WINDOW:?}",
        metrics.over_limit, metrics.groups, metrics.max_latency
    );
}
