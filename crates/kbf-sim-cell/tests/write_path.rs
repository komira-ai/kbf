//! The direct write path (no local tier) under the object store's faults and crashes
//! at each point of the path: the scenarios of docs/design/write-tier-scenarios.md
//! that can run before the tier exists. Every run checks properties 1 to 9 after
//! every step (crates/kbf-sim-cell/src/check.rs); these tests add what each scenario
//! must show beyond them, and that its fault was met.

use std::time::Duration;

use kbf_sim_cell::{
    Down, Kind, RunReport, Scenario, ServerFault, StoreFault, StorePoint, Sweep, sweep,
};
use kbf_types::FarmTime;

const SEEDS: u64 = 64;

fn swept(kind: Kind, seeds: u64) -> Sweep {
    let swept = sweep(0..seeds, |seed| Scenario::new(kind, seed));
    swept.assert_clean();
    assert_eq!(swept.reports.len() as u64, seeds);
    swept
}

/// The outage window of a store-outage scenario.
fn outage(seed: u64, kind: Kind) -> (FarmTime, FarmTime, StoreFault) {
    let s = Scenario::new(kind, seed);
    let (from, fault) = s.store.faults[0];
    (from, fault.until().expect("an outage has an end"), fault)
}

/// An outage of minutes: the direct path acknowledges nothing while the store is
/// down (an ack would name bytes no store holds), every action is answered once it
/// returns, and the outage met PUTs.
///
/// Catches an ack before the store has the bytes (property 8, and the window
/// assertion: mutant "ack on `Upload`"), a blob indexed before or without its PUT
/// (property 9: mutants "commit `PutBlob` at `Upload`", "index on `PutFailed`"), and
/// a store whose pause or refusal does nothing (mutant "a pause holds nothing").
#[test]
fn a_store_outage_of_minutes_never_acks_an_unstored_blob() {
    // Half the usual seeds: a run is minutes long, and property 1 reads the whole
    // durable log back at each step that changes it.
    check_outage(Kind::StoreOutage, SEEDS / 2);
}

/// The same for over an hour (seed 0 hangs, seed 1 refuses): the backlog drains after.
#[test]
fn a_store_outage_of_hours_never_acks_an_unstored_blob() {
    check_outage(Kind::StoreOutageHours, 2);
}

fn check_outage(kind: Kind, seeds: u64) {
    let swept = swept(kind, seeds);
    let mut holds = 0;
    for (seed, report) in &swept.reports {
        let (from, until, fault) = outage(*seed, kind);
        let met = match fault {
            StoreFault::Pause { .. } => {
                holds += 1;
                report.store.held
            }
            _ => report.store.failed,
        };
        assert!(met > 0, "{kind:?} seed {seed} met no PUT: {report:?}");
        // A PUT answered just before the outage began may be acknowledged at its start.
        let settled = from.saturating_add(Duration::from_secs(1));
        let inside = report
            .acked
            .iter()
            .find(|at| **at > settled && **at < until);
        assert_eq!(
            inside, None,
            "{kind:?} seed {seed}: acked inside {from:?}..{until:?}"
        );
        assert!(
            report.acked.iter().any(|at| *at >= until),
            "{kind:?} seed {seed}"
        );
    }
    // Both forms of outage were met: a store that hangs and one that refuses.
    assert!(
        holds > 0 && holds < seeds,
        "{holds} of {seeds} outages hold"
    );
}

/// A store answering seconds late (past the client's retry, so one blob has several
/// PUTs in flight): every property holds and every slowed PUT is answered.
///
/// Catches a late answer indexed under another PUT's object (property 9), and a
/// slow window that slows nothing.
#[test]
fn a_slow_store_keeps_every_property() {
    for (seed, report) in &swept(Kind::StoreSlow, SEEDS).reports {
        assert!(report.store.slowed > 0, "seed {seed}: {report:?}");
    }
}

/// PUTs failing here and there, some after the store kept the object: nothing is
/// indexed from a failed PUT that kept nothing, and an object the store kept but
/// reported failed is left for the orphan sweep, never named by the index.
///
/// Catches indexing a failed PUT (property 9: mutant "index on `PutFailed`") and an
/// ack of a failed PUT (property 8).
#[test]
fn store_errors_index_nothing_they_failed() {
    let swept = swept(Kind::StoreErrors, SEEDS);
    let failed: u64 = swept.each().map(|r| r.store.failed).sum();
    let false_fails: u64 = swept.each().map(|r| r.store.false_fails).sum();
    assert!(
        failed > SEEDS && false_fails > SEEDS,
        "{failed} {false_fails}"
    );
    for (seed, report) in &swept.reports {
        if report.store.false_fails > 0 {
            assert!(report.orphans > Some(0), "seed {seed}: {report:?}");
        }
    }
}

/// The store keeps objects but loses their acknowledgement: the client's retry is a
/// new PUT under a new id, the blob is indexed once, and the unacknowledged object is
/// an orphan.
///
/// Catches a lost ack that leaves a blob never stored (property 7) or indexed twice
/// under clashing ids (property 9).
#[test]
fn a_lost_store_ack_leaves_an_orphan_not_a_hole() {
    for (seed, report) in &swept(Kind::StoreLostAck, SEEDS).reports {
        assert!(report.store.lost_acks > 0, "seed {seed}: {report:?}");
        assert!(report.orphans > Some(0), "seed {seed}: {report:?}");
    }
}

/// The server killed or cut off at a random point of the write path, a random reach:
/// properties 1 to 9 hold across the restart.
///
/// Catches a restart that reuses the last start's object ids (property 9: mutant "a
/// restart reuses the writer epoch"), and an ack sent before the commit (property 8:
/// mutant "ack before `PutBlob`").
#[test]
fn crashes_on_the_write_path_leave_nothing_half_indexed() {
    let swept = swept(Kind::StoreCrash, SEEDS);
    let crashed = swept.each().filter(|r| r.crashes > 0).count() as u64;
    assert!(crashed * 4 >= SEEDS * 3, "{crashed} of {SEEDS} crashed");
    for (seed, report) in &swept.reports {
        assert_eq!(report.starts, 1 + report.crashes as u64, "seed {seed}");
    }
}

/// The scenario `seed` draws, with one crash at `point` at 10 s, going `down`.
fn crash_at(seed: u64, point: StorePoint, down: Down) -> Scenario {
    let mut s = Scenario::new(Kind::StoreCrash, seed);
    let crash = ServerFault::CrashAt {
        point,
        nth: 0,
        down,
    };
    s.server.faults = vec![(FarmTime::from_millis(10_000), crash)];
    s
}

/// Each point of the write path, under a kill and under power cuts keeping nothing
/// and everything unsynced: the server goes down there exactly once, comes back, and
/// leaves an orphan (the PUT's object, or the retry's), never an entry without bytes.
///
/// Catches a crash point that never fires (mutant "the in-commit crash is not armed"),
/// and every defect the sweep above catches, at each point deterministically.
#[test]
fn every_write_path_crash_point_leaves_nothing_half_indexed() {
    let points = [
        StorePoint::PutSent,
        StorePoint::BeforeCommit,
        StorePoint::InCommit { after: 0 },
        StorePoint::InCommit { after: 1 },
        StorePoint::BeforeReply,
    ];
    let downs = [
        Down::Kill,
        Down::PowerCut { keep: 0, names: 0 },
        Down::PowerCut {
            keep: usize::MAX,
            names: u64::MAX,
        },
    ];
    for point in points {
        for down in downs {
            let swept = sweep(0..4, |seed| crash_at(seed, point, down));
            swept.assert_clean();
            for (seed, r) in &swept.reports {
                let at = format!("{point:?} {down:?} seed {seed}: {r:?}");
                assert_eq!((r.crashes, r.starts), (1, 2), "{at}");
                assert!(r.orphans > Some(0), "{at}");
            }
        }
    }
}

/// Catches a store fault or crash point that reads a clock or an unordered map: a
/// sweep replays to the same traces.
#[test]
fn a_store_fault_sweep_replays() {
    let traces = |s: Sweep| -> Vec<_> { s.each().map(|r: &RunReport| r.trace).collect() };
    for kind in [Kind::StoreCrash, Kind::StoreSlow] {
        let a = traces(sweep(0..3, |seed| Scenario::new(kind, seed)));
        assert_eq!(a, traces(sweep(0..3, |seed| Scenario::new(kind, seed))));
        assert_eq!(a.len(), 3);
    }
}
