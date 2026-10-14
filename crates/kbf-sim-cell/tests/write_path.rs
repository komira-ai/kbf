//! The direct write path (no local tier) under the object store's faults and crashes
//! at each point of the path: the scenarios of docs/design/write-tier-scenarios.md
//! that can run before the tier exists. Every run checks properties 1 to 9 after
//! every step (crates/kbf-sim-cell/src/check.rs); these tests add what each scenario
//! must show beyond them, and that its fault was met.

use std::time::Duration;

use kbf_meta::ObjectId;
use kbf_sim_cell::{Down, Kind, Met, RunReport, Scenario, StoreFault, StorePoint, Sweep, sweep};
use kbf_types::Digest;
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
            .find(|(at, _, _)| *at > settled && *at < until);
        assert_eq!(
            inside, None,
            "{kind:?} seed {seed}: acked inside {from:?}..{until:?}"
        );
        assert!(
            report.acked.iter().any(|(at, _, _)| *at >= until),
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

/// The PUTs of `report` a fault met `how`, and whether the server's own record of
/// that PUT (`seen`) shows the fault: the outcome only the fault causes, not one a
/// network duplicate of a PUT or an upload can cause too.
fn met<'a>(
    report: &'a RunReport,
    how: Met,
    seen: &'a std::collections::BTreeSet<ObjectId>,
) -> impl Iterator<Item = (ObjectId, Digest, bool)> + 'a {
    report
        .met
        .iter()
        .filter(move |(_, _, m)| *m == how)
        .map(move |(o, d, _)| (*o, *d, seen.contains(o)))
}

/// The object `object` held `digest`, and the blob was acknowledged all the same,
/// under another object: the fault cost a retry, not the blob.
fn retried_elsewhere(report: &RunReport, object: ObjectId, digest: &Digest) -> bool {
    report
        .acked
        .iter()
        .any(|(_, d, o)| d == digest && *o != object)
}

/// PUTs failing here and there, some after the store kept the object: a PUT the
/// store refused is answered `UNAVAILABLE` and leaves nothing; one it kept but
/// answered failed is answered `UNAVAILABLE` too, and its object is an orphan nothing
/// indexes; each blob is acknowledged after, under another PUT's object.
///
/// Catches indexing a failed PUT (property 9: mutant "index on `PutFailed`"), an ack
/// of a failed PUT (property 8), and a store whose false failure answers done (the
/// server's refusals: mutant "a kept-but-failed PUT answers `PutDone`").
#[test]
fn store_errors_index_nothing_they_failed() {
    let swept = swept(Kind::StoreErrors, SEEDS);
    let (mut refused, mut false_fails) = (0, 0);
    for (seed, r) in &swept.reports {
        let orphans = r.orphans.as_ref().expect("the server is up at the end");
        let at = format!("seed {seed}: {r:?}");
        for (object, digest, answered) in met(r, Met::Failed, &r.put_failed) {
            refused += u64::from(answered);
            assert!(!orphans.contains(&object), "{object} kept: {at}");
            assert!(retried_elsewhere(r, object, &digest), "{object}: {at}");
        }
        let false_failed: Vec<_> = met(r, Met::FalseFail, &r.put_failed).collect();
        for (object, digest, answered) in &false_failed {
            if *answered {
                assert!(orphans.contains(object), "{object} not an orphan: {at}");
                assert!(retried_elsewhere(r, *object, digest), "{object}: {at}");
            }
        }
        let answered = false_failed.iter().filter(|(_, _, a)| *a).count() as u64;
        if !false_failed.is_empty() {
            assert!(answered > 0, "no false failure reached the server: {at}");
        }
        false_fails += answered;
    }
    assert!(
        refused > SEEDS && false_fails > SEEDS,
        "{refused} {false_fails}"
    );
}

/// The store keeps objects but loses their acknowledgement: the PUT's deadline
/// answers the upload `UNAVAILABLE`, the client's retry is a new PUT under a new id
/// that is acknowledged, and the unanswered object is an orphan.
///
/// Every lost ack times out, unless a network duplicate of its PUT was answered (the
/// store saw the object twice); each seed has one that timed out unless all were
/// duplicated, and most lost acks time out.
///
/// Catches a lost ack that leaves a blob never stored (property 7) or indexed under
/// an object that lacks it (property 9), and a lost ack that is answered after all
/// (mutant "a lost ack still sends `PutDone`": no deadline fires for it).
#[test]
fn a_lost_store_ack_times_out_and_leaves_an_orphan_not_a_hole() {
    let mut timed_out = 0;
    let mut lost = 0;
    for (seed, r) in &swept(Kind::StoreLostAck, SEEDS).reports {
        let orphans = r.orphans.as_ref().expect("the server is up at the end");
        let at = format!("seed {seed}: {r:?}");
        for (object, digest, timed) in met(r, Met::LostAck, &r.timed_out) {
            lost += 1;
            if !timed {
                assert!(r.repeated.contains(&object), "{object} answered: {at}");
                continue;
            }
            timed_out += 1;
            assert!(orphans.contains(&object), "{object} not an orphan: {at}");
            assert!(retried_elsewhere(r, object, &digest), "{object}: {at}");
        }
        assert!(r.store.lost_acks > 0, "{at}");
    }
    assert!(
        timed_out * 10 >= lost * 9 && lost > SEEDS,
        "{timed_out} of {lost}"
    );
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

/// Whether a `PutBlob` commit cut at its `after`-th filesystem operation is in the
/// index at the next start. Operation 0 is the record's append, operation 1 its sync:
/// a crash at the append writes nothing; at the sync, the record is written but not
/// synced, so a kill keeps it, a power cut keeping all of the unsynced tail keeps it,
/// and one keeping none of it or part of it (a torn record) loses it.
fn survives(after: u64, down: Down) -> bool {
    match (after, down) {
        (0, _) => false,
        (_, Down::Kill) => true,
        (_, Down::PowerCut { keep, .. }) => keep == usize::MAX,
    }
}

/// Each point of the write path, under a kill and under power cuts keeping none, part
/// and all of the unsynced bytes: the server goes down there exactly once, comes back,
/// and leaves an orphan (the PUT's object, or the retry's), never an entry without
/// bytes. A crash inside the `PutBlob` commit lands in that commit, and the next start
/// has the blob indexed exactly when the commit's record was whole on the disk.
///
/// Catches a crash point that never fires (mutant "the in-commit crash is not
/// armed"), an in-commit crash that lands in a later commit (mutant "planted 40
/// operations on"), a power cut that keeps what a kill keeps (mutant "`PowerCut`
/// handled as `Kill`"), a torn record read back as an entry (mutant "open keeps a
/// torn tail", property 1), and every defect the sweep above catches, at each point
/// deterministically.
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
        Down::PowerCut { keep: 1, names: 0 },
        Down::PowerCut {
            keep: usize::MAX,
            names: u64::MAX,
        },
    ];
    for point in points {
        for down in downs {
            let swept = sweep(0..4, |seed| Scenario::crash_at(seed, point, down));
            swept.assert_clean();
            for (seed, r) in &swept.reports {
                let at = format!("{point} {down} seed {seed}: {r:?}");
                assert_eq!((r.crashes, r.starts), (1, 2), "{at}");
                assert!(r.orphans.as_ref().is_some_and(|o| !o.is_empty()), "{at}");
                let Some(after) = point.in_commit() else {
                    assert!(r.cuts.is_empty(), "{at}");
                    continue;
                };
                assert_eq!(r.cuts.len(), 1, "{at}");
                let cut = r.cuts[0];
                assert_eq!(cut.down, down, "{at}");
                if let Down::PowerCut { keep: 1, .. } = down {
                    // Part of the record: a torn one.
                    assert!(cut.unsynced > 1 || after == 0, "{at}");
                }
                assert_eq!(cut.survived, Some(survives(after, down)), "{at}");
            }
        }
    }
}

/// A crash planted past the `PutBlob` commit's last operation (it has two) lands in a
/// later commit: the server goes down there, but no cut of `PutBlob` is recorded.
///
/// Catches a crash recorded where it was planted rather than where it fired (the
/// earlier code recorded it at arming), and a commit that grew an operation unnoticed.
#[test]
fn an_in_commit_crash_past_the_commit_is_no_cut_of_it() {
    let point = StorePoint::InCommit { after: 2 };
    let swept = sweep(0..4, |seed| Scenario::crash_at(seed, point, Down::Kill));
    swept.assert_clean();
    for (seed, r) in &swept.reports {
        let at = format!("seed {seed}: {r:?}");
        assert_eq!((r.crashes, r.starts, r.cuts.len()), (0, 2, 0), "{at}");
    }
}

/// Catches a store fault or crash point that reads a clock or an unordered map: a
/// sweep replays to the same traces.
#[test]
fn a_store_fault_sweep_replays() {
    let traces = |s: Sweep| -> Vec<_> { s.each().map(|r: &RunReport| r.trace).collect() };
    let kinds = [
        Kind::StoreCrash,
        Kind::StoreSlow,
        Kind::StoreErrors,
        Kind::StoreLostAck,
    ];
    for kind in kinds {
        let a = traces(sweep(0..3, |seed| Scenario::new(kind, seed)));
        assert_eq!(a, traces(sweep(0..3, |seed| Scenario::new(kind, seed))));
        assert_eq!(a.len(), 3);
    }
}
