//! The local write tier in the cell (rollout stage 5 of docs/design/write-path.md): the
//! scenarios of docs/design/write-tier-scenarios.md that the tier of stage 4b makes
//! testable, each a seed sweep. Every run checks properties 1 to 13 after every step
//! (crates/kbf-sim-cell/src/check.rs; 10 to 13 and the tier halves of 5, 8 and 9 are
//! the tier's invariants C, D, E and F, and A and B); these tests add what each
//! scenario must show beyond them, and that its fault was met.

use std::time::Duration;

use kbf_sim_cell::{Down, Kind, RunReport, Scenario, Sweep, TierPoint, sweep};
use kbf_tier::block_name;
use kbf_types::FarmTime;

fn swept(kind: Kind, seeds: u64) -> Sweep {
    let swept = sweep(0..seeds, |seed| Scenario::new(kind, seed));
    swept.assert_clean();
    assert_eq!(swept.reports.len() as u64, seeds);
    swept
}

/// How many of `swept`'s seeds `met` holds for.
fn count(swept: &Sweep, met: impl Fn(&RunReport) -> bool) -> usize {
    swept.each().filter(|r| met(r)).count()
}

/// The tier with only the network's faults: uploads are staged and acknowledged from
/// the tier, blocks are sealed and flushed, reads are served, nothing is left staged
/// (13), and a binary that runs only machine version 1 refuses the log at the raise,
/// before any tier command, while this one opens it again to the same staged table
/// (scenario 30).
///
/// Catches a log that is never raised (mutant "the machine keeps format 1": no
/// `PutStaged` commits, so nothing is staged), a flusher that never runs, and a
/// rollback that opens a raised log (mutant "a version past what this binary runs is
/// skipped").
#[test]
fn the_tier_stages_flushes_and_refuses_a_rollback() {
    let swept = swept(Kind::TierCalm, 16);
    for (seed, r) in &swept.reports {
        let at = format!("seed {seed}: {:?}", r.tier);
        assert!(r.tier.staged_acks > 0, "nothing staged: {at}");
        assert!(!r.tier.flushed.is_empty(), "nothing flushed: {at}");
        assert!(r.tier.reads > 0, "nothing read: {at}");
        let rollback = r.rollback.as_ref().expect("the server is up at the end");
        let raised = rollback.raised_at.expect("the log was raised");
        let first = rollback.first_tier_command.expect("a tier command");
        assert!(raised < first, "seed {seed}: {rollback:?}");
        let refused = rollback.refused.as_deref().unwrap_or("opened");
        let at_raise = format!("raft log: entry {raised}: ");
        assert!(refused.starts_with(&at_raise), "seed {seed}: {refused}");
        assert!(rollback.reopened, "seed {seed}: {rollback:?}");
    }
}

/// One crash at `point`, going `down`, over `seeds` seeds: the crash fired in most of
/// them, and each crash's next start did what the scenario expects of it.
fn crash_at(point: TierPoint, down: Down, seeds: u64) -> usize {
    let swept = sweep(0..seeds, |seed| Scenario::tier_crash_at(seed, point, down));
    swept.assert_clean();
    let mut fired = 0;
    for (seed, r) in &swept.reports {
        let at = format!("{point}/{down} seed {seed}: {:?}", r.tier.cuts);
        let Some(cut) = r.tier.cuts.first() else {
            continue;
        };
        fired += 1;
        assert_eq!(cut.point, point, "{at}");
        let recovery = cut
            .recovery
            .as_ref()
            .unwrap_or_else(|| panic!("no restart: {at}"));
        let object = cut.object.expect("a cut names its object");
        let removed = recovery.removed.contains(&block_name(object));
        let truncated = recovery.truncated.contains(&object);
        assert!(!recovery.lost.contains(&object), "{at}");
        match point {
            // Synced but never indexed: an orphan block deleted, or the bytes past
            // a shared block's committed length cut off (scenarios 2 and 21).
            TierPoint::AfterSync => assert!(removed || truncated, "{at}: {recovery:?}"),
            // `ObjectFlushed` committed: the `.blk` is an orphan the sweep deletes, the
            // store holds the object (scenario 21).
            TierPoint::BeforeRename => assert!(removed, "{at}: {recovery:?}"),
            // Indexed and durable: the block is kept whole (scenarios 4 and 5).
            TierPoint::BeforeReply | TierPoint::Acked => {
                assert!(!removed && !truncated, "{at}: {recovery:?}");
            }
            // The store has the object, `ObjectFlushed` is lost: the next start flushes
            // it again under the same id, with the same bytes (scenarios 11 and 12;
            // property 10 checks the bytes).
            TierPoint::AfterFlushPut => {
                assert!(r.repeated.contains(&object), "{at}: not flushed again");
                assert!(r.tier.flushed.contains(&object), "{at}");
            }
            // Either `ObjectFlushed` survived (the block is an orphan) or it did not
            // (the object is flushed again).
            TierPoint::InFlushCommit { .. } => {
                assert!(removed || r.tier.flushed.contains(&object), "{at}");
            }
            TierPoint::AfterAppend | TierPoint::InPutStaged { .. } => {}
        }
    }
    fired
}

const DOWNS: [Down; 3] = [
    Down::Kill,
    Down::PowerCut { keep: 0, names: 0 },
    Down::PowerCut {
        keep: usize::MAX,
        names: u64::MAX,
    },
];

/// Every crash point of the tier's write and flush paths (scenarios 1 to 5, 11, 12,
/// 21), each under a kill and power cuts keeping none and all of the unsynced bytes
/// and names: what each next start does with the cut block, and every property.
///
/// Catches an ack before `fdatasync` (mutant "sync skips the `fdatasync`") and before
/// the block's name is synced (mutant "no `sync_dir`"): a power cut after the ack
/// loses the block (property 8); a start without the orphan sweep (property 12, and
/// the `AfterSync` and `BeforeRename` rows); a start that keeps bytes past the
/// committed length (12); recovered unsealed blocks never sealed (13); and the rename
/// to a flushed copy before `ObjectFlushed` commits (a crash between them leaves a
/// staged object without its block: 8).
#[test]
fn every_tier_crash_point_keeps_every_property() {
    let points = TierPoint::ALL.into_iter().chain([
        TierPoint::InPutStaged { after: 1 },
        TierPoint::InFlushCommit { after: 1 },
    ]);
    for point in points {
        let mut fired = 0;
        for down in DOWNS {
            fired += crash_at(point, down, 4);
        }
        assert!(fired >= 8, "{point}: {fired} of 12 runs crashed");
    }
}

/// Crashes at random tier points, kills and power cuts, while the store is slow.
#[test]
fn tier_crash_seeds_keep_every_property() {
    let swept = swept(Kind::TierCrash, 32);
    let crashed = count(&swept, |r| !r.tier.cuts.is_empty());
    assert!(crashed >= 16, "{crashed} of 32 crashed");
}

/// The window of a tier outage scenario.
fn window(kind: Kind, seed: u64) -> (FarmTime, FarmTime) {
    let s = Scenario::new(kind, seed);
    let (from, fault) = s.store.faults[0];
    (from, fault.until().expect("an outage ends"))
}

/// A store outage with the tier (scenarios 6 and 7): acknowledgements go on from the
/// tier while it has room, and every one inside the outage is a staged one; once the
/// budget is reached writes take the direct path, which the store refuses
/// (backpressure, never loss); the backlog drains once the store is back.
///
/// Catches a budget that is not enforced (mutant "`check_budget` admits anything":
/// property 11), an ack inside the outage that the tier did not stage (a direct ack
/// with no store), and an outage the tier never filled (a test that proves nothing).
#[test]
fn a_store_outage_fills_the_tier_then_pushes_back() {
    for (kind, seeds) in [(Kind::TierStoreOutage, 16), (Kind::TierStoreOutageHours, 2)] {
        let swept = swept(kind, seeds);
        for (seed, r) in &swept.reports {
            let (from, until) = window(kind, *seed);
            let settled = from.saturating_add(Duration::from_secs(1));
            let inside: Vec<_> = r
                .acked
                .iter()
                .filter(|(at, _, _)| *at > settled && *at < until)
                .collect();
            for (at, _, object) in &inside {
                let staged = r.tier.staged.contains(object);
                assert!(staged, "{kind:?} seed {seed}: direct ack at {at:?}");
            }
            assert!(
                r.tier.full > 0,
                "{kind:?} seed {seed}: the budget never pushed back"
            );
            assert!(
                r.acked.iter().any(|(at, _, _)| *at >= until),
                "{kind:?} {seed}"
            );
        }
        let acked_inside = count(&swept, |r| r.tier.staged_acks > 0);
        assert_eq!(acked_inside as u64, seeds, "{kind:?}");
    }
}

/// The tier's disk full (`ENOSPC`) below its budget, or failing (scenarios 13 and
/// 14's read error): a write that met it is not acknowledged from the tier and goes
/// direct; a flush whose re-read failed is retried; nothing is lost.
///
/// Catches a write error swallowed (mutant "`write` answers `Ok` when the disk
/// refused": the range is synced and acknowledged without its bytes, property 8),
/// and faults that never met a write or a flush (a test that proves nothing).
#[test]
fn a_full_disk_sends_writes_direct_and_loses_nothing() {
    let swept = swept(Kind::TierDiskFull, 32);
    let met = count(&swept, |r| r.tier.disk_errors > 0);
    assert!(met >= 8, "{met} of 32 seeds met a full disk on a write");
    let retried = count(&swept, |r| r.tier.reread_failures > 0);
    assert!(retried > 0, "no flush met a failed re-read");
}

/// A bad sector in a staged block before its flush (scenario 14): the flusher's
/// re-read finds the damaged record, the object is marked corrupt and never PUT, its
/// blobs answer unavailable (never absent), and no read serves the damaged bytes.
///
/// Catches a flusher that uploads without checking the records (mutant "the check
/// passes every record": the damaged object reaches the store), and a bad sector that
/// never landed in a block (a test that proves nothing).
#[test]
fn a_bad_block_is_marked_corrupt_and_never_flushed() {
    let swept = swept(Kind::TierBadBlock, 32);
    let mut damaged = 0;
    for (seed, r) in &swept.reports {
        for object in &r.tier.damaged {
            damaged += 1;
            assert!(r.tier.corrupt.contains(object), "seed {seed}: {object}");
            assert!(!r.tier.flushed.contains(object), "seed {seed}: {object}");
        }
    }
    assert!(damaged >= 16, "{damaged} of 32 seeds damaged a block");
}

/// A budget that holds little beyond the staged blocks (scenario 16): flushed copies
/// are evicted, reads that found the copy gone fall back to the store, and no read of
/// a present blob comes back empty.
///
/// Catches eviction of a block whose only copy is local (mutant "`make_room` evicts
/// staged blocks too": the read finds the bytes nowhere, properties 8 and 9), and a
/// race the runs never met.
#[test]
fn eviction_racing_a_read_falls_back_to_the_store() {
    let swept = swept(Kind::TierEviction, 32);
    let evicted = count(&swept, |r| r.tier.evicted > 0);
    assert!(evicted >= 24, "{evicted} of 32 seeds evicted a copy");
    let raced: u64 = swept.each().map(|r| r.tier.fallbacks).sum();
    assert!(raced > 0, "no read lost its local copy to eviction");
}
