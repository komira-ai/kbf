//! A simulation of every crash point: one scripted workload (grouped ranges committed
//! in and out of order, a seal at the segment limit, a seal by delay, segment
//! uploads, a range synced but never committed, an upload never synced) is run once
//! per filesystem operation with a crash or an I/O error planted at that operation.
//! Each run's disk is then cut as a power cut could leave it (torn tails of 0, 1 or
//! every unsynced byte; no, all, or each single one of the unsynced name changes) and
//! as a process crash leaves it, and opened again as the next start. A second sweep
//! crashes that recovery itself at each of its operations and opens once more.
//!
//! The invariants, after every cut:
//! - the start succeeds and reports nothing lost: no acknowledged block is missing;
//! - every acknowledged range reads back its exact bytes;
//! - the directory holds a block for exactly the staged objects (no orphan survives)
//!   and each is exactly its committed length (no torn or uncommitted tail survives);
//! - the budget counts exactly the staged bytes.
//!
//! Catches what the single-scenario tests in `restart.rs` and `tier.rs` catch, at
//! interleavings they do not script: acknowledging before the file or directory
//! sync, a torn tail served or kept, an orphan left, a crash inside recovery that
//! leaves a state the next recovery cannot read.

mod support;

use std::time::Instant;

use kbf_meta::ObjectId;
use kbf_store::{Fault, FaultFs};
use kbf_tier::{TierError, TierOptions, WriteTier, block_name};

use support::{Index, SEAL_AFTER, ack_group, ack_upload, blocks, bytes, open_tier, options};

/// A segment limit small enough for the workload to fill a grouped block.
fn opts() -> TierOptions {
    options(1 << 20, 32)
}

/// An acknowledged range and its bytes.
#[derive(Clone, Debug)]
struct Acked {
    object: ObjectId,
    offset: u64,
    data: Vec<u8>,
}

/// One acknowledged grouped range, recorded.
fn ack(
    tier: &dyn WriteTier,
    index: &mut Index,
    acked: &mut Vec<Acked>,
    now: Instant,
    data: Vec<u8>,
) -> Result<ObjectId, TierError> {
    let (object, offset) = ack_group(tier, index, &data, now)?;
    acked.push(Acked {
        object,
        offset,
        data,
    });
    Ok(object)
}

/// The workload, as start 1 of `fs`. Stops at the first error: after a planted
/// crash every operation fails, and after a planted error the caller has answered
/// what it could. Returns how far it got only through `index` and `acked`.
fn workload(fs: &FaultFs, index: &mut Index, acked: &mut Vec<Acked>) -> Result<(), TierError> {
    let t0 = Instant::now();
    let (tier, _) = open_tier(fs, 1, opts(), index)?;
    let first = ack(&tier, index, acked, t0, bytes(1, 10))?;

    let a = tier.reserve_group(8, t0)?;
    let b = tier.reserve_group(8, t0)?;
    tier.write(&b, 0, &bytes(3, 8))?;
    tier.write(&a, 0, &bytes(2, 8))?;
    for (r, tag) in [(b, 3), (a, 2)] {
        let synced = tier.sync(r).map_err(|u| u.error)?;
        let (object, offset) = (synced.reservation().object(), synced.reservation().offset());
        index.put_staged(&synced, false);
        tier.committed(synced);
        acked.push(Acked {
            object,
            offset,
            data: bytes(tag, 8),
        });
    }

    let upload = ack_upload(&tier, index, &bytes(4, 9))?;
    acked.push(Acked {
        object: upload,
        offset: 0,
        data: bytes(4, 9),
    });

    let second = ack(&tier, index, acked, t0, bytes(5, 8))?;
    assert_ne!(first, second, "the first block reached the segment limit");
    for object in tier.poll_seals(t0) {
        index.seal(object);
        tier.sealed(object)?;
    }

    ack(&tier, index, acked, t0, bytes(9, 2))?;
    let torn = tier.reserve_group(4, t0)?;
    tier.write(&torn, 0, &bytes(6, 4))?;
    std::mem::forget(tier.sync(torn).map_err(|u| u.error)?);

    let orphan = tier.reserve_upload(5)?;
    tier.write(&orphan, 0, &bytes(7, 5))?;
    std::mem::forget(tier.sync(orphan).map_err(|u| u.error)?);

    let open = tier.reserve_upload(3)?;
    tier.write(&open, 0, &bytes(8, 3))?;
    std::mem::forget(open);

    let later = t0 + SEAL_AFTER;
    for object in tier.poll_seals(later) {
        index.seal(object);
        tier.sealed(object)?;
    }
    Ok(())
}

/// Opens `cut` as the next start and checks every invariant of the module docs.
fn check(cut: &FaultFs, index: &Index, acked: &[Acked], what: &str) {
    let (tier, rec) = open_tier(cut, 2, opts(), index)
        .unwrap_or_else(|e| panic!("{what}: the start after a crash failed: {e}"));
    assert_eq!(rec.lost, [], "{what}: {rec:?}");
    for a in acked {
        let got = tier.read(a.object, a.offset, a.data.len());
        assert_eq!(got.ok().as_deref(), Some(&a.data[..]), "{what}: {a:?}");
    }
    let staged = index.staged();
    let mut want: Vec<String> = staged.iter().map(|s| block_name(s.object)).collect();
    want.sort();
    assert_eq!(blocks(cut), want, "{what}: orphans");
    for s in &staged {
        let len = cut.contents(&block_name(s.object)).map(|c| c.len() as u64);
        assert_eq!(len, Some(s.len), "{what}: {s:?}");
    }
    let used: u64 = staged.iter().map(|s| s.len).sum();
    assert_eq!(tier.usage().used, used, "{what}: budget");
}

/// Every cut of `fs` the module docs list: `(label, disk)`.
fn cuts(fs: &FaultFs) -> Vec<(String, FaultFs)> {
    let names = fs.pending_names().min(64);
    let mut masks = vec![0, u64::MAX];
    masks.extend((0..names).map(|bit| 1u64 << bit));
    let mut out = vec![("restart".to_owned(), fs.restart())];
    for keep in [0, 1, usize::MAX] {
        for &mask in &masks {
            out.push((
                format!("keep {keep} names {mask:#x}"),
                fs.crash_reordered(keep, mask),
            ));
        }
    }
    out
}

fn ops_of_workload() -> u64 {
    let fs = FaultFs::new();
    workload(&fs, &mut Index::default(), &mut Vec::new()).unwrap();
    fs.ops()
}

#[test]
fn every_crash_point_of_the_workload_recovers() {
    let total = ops_of_workload();
    assert!(total > 30, "the workload runs {total} operations");
    let mut acked_at_end = 0;
    for op in 0..=total {
        for fault in [Fault::Crash, Fault::Error] {
            let fs = FaultFs::new();
            fs.fail_at(op, fault);
            let (mut index, mut acked) = (Index::default(), Vec::new());
            let finished = workload(&fs, &mut index, &mut acked).is_ok();
            assert_eq!(finished, op == total, "op {op} {fault:?}");
            acked_at_end = acked_at_end.max(acked.len());
            for (label, cut) in cuts(&fs) {
                check(&cut, &index, &acked, &format!("op {op} {fault:?} {label}"));
            }
        }
    }
    assert_eq!(
        acked_at_end, 6,
        "the run without a fault acknowledges every range"
    );
}

#[test]
fn every_crash_point_of_a_recovery_recovers() {
    let fs = FaultFs::new();
    let (mut index, mut acked) = (Index::default(), Vec::new());
    workload(&fs, &mut index, &mut acked).unwrap();
    for (label, disk) in cuts(&fs) {
        let ops_of_open = {
            let probe = disk.restart();
            let start = probe.ops();
            open_tier(&probe, 2, opts(), &index).unwrap();
            probe.ops() - start
        };
        for op in 0..ops_of_open {
            let attempt = disk.restart();
            attempt.fail_at(attempt.ops() + op, Fault::Crash);
            assert!(open_tier(&attempt, 2, opts(), &index).is_err());
            for (again, cut) in cuts(&attempt) {
                check(
                    &cut,
                    &index,
                    &acked,
                    &format!("{label}, open op {op}, {again}"),
                );
            }
        }
    }
}
