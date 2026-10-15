//! `EvictObjects`: forgetting objects the store no longer holds, the blobs whose current
//! location is in one of them, and the action-cache entries that need such a blob.

mod common;

use std::time::Instant;

use common::{
    ANY_INDEX, cache_action, digest, in_segment, mark, mark_missing, meta, object, put, put_action,
};
use kbf_meta::{
    ActionAnswer, ActionRecord, Applied, BlobAnswer, BlobWrite, Closure, Command, Epoch, Evicted,
    Location, MetaState, Miss, ObjectId, Role, ServerId, StagedPut, StoreId, TierId,
    UnreachableReason,
};
use kbf_types::{Digest, DigestFunction};

fn evict(m: &mut MetaState, objects: &[ObjectId]) -> Evicted {
    let objects = objects.iter().copied().collect();
    match m.execute(ANY_INDEX, Command::EvictObjects { objects }) {
        Applied::Evicted(e) => e,
        other => panic!("EvictObjects applied as {other:?}"),
    }
}

fn hit(m: &MetaState, action: &Digest) -> bool {
    matches!(m.action(action), ActionAnswer::Hit { .. })
}

fn record(result: Digest, closure: &[Digest]) -> ActionRecord {
    ActionRecord {
        result,
        closure: closure.iter().copied().collect::<Closure>(),
    }
}

/// Catches: an eviction that leaves the action-cache entries naming a removed blob
/// (mutant: no action scan), so a hit would wait for the store to answer for an
/// object it no longer has; one that removes only the first blob of an object, or a
/// blob of an object not named; and one that drops an entry whose blobs all stay.
#[test]
fn evict_drops_blobs_and_naming_actions() {
    let mut m = meta();
    // Action A: result in segment 1, output 2, tree 3, child 4.
    let a = cache_action(&mut m);
    // A second blob in segment 2, the result of action B; action C keeps only its
    // closure there (A's output); action D lives in segments 5 and 6.
    let (b_result, c_result, d_result, d_output) = (digest(20), digest(21), digest(22), digest(23));
    put(&mut m, b_result, in_segment(2, 1));
    put(&mut m, c_result, in_segment(5, 1));
    put(&mut m, d_result, in_segment(5, 2));
    put(&mut m, d_output, in_segment(6, 0));
    let (b, c, d) = (digest(30), digest(31), digest(32));
    put_action(&mut m, Role::Daemon, b, record(b_result, &[])).expect("B");
    put_action(&mut m, Role::Daemon, c, record(c_result, &[a.output])).expect("C");
    put_action(&mut m, Role::Daemon, d, record(d_result, &[d_output])).expect("D");
    let (blobs, actions) = (m.blob_count(), m.action_count());

    let evicted = evict(&mut m, &[object(2)]);

    let mut want_blobs = vec![a.output, b_result];
    want_blobs.sort();
    let mut want_actions = vec![a.action, b, c];
    want_actions.sort();
    assert_eq!(
        evicted,
        Evicted {
            blobs: want_blobs,
            actions: want_actions,
        }
    );
    assert_eq!(m.blob(&a.output), BlobAnswer::Absent);
    assert_eq!(m.blob(&b_result), BlobAnswer::Absent);
    for action in [a.action, b, c] {
        assert_eq!(
            m.action(&action),
            ActionAnswer::Miss(Miss::NoEntry),
            "{action}"
        );
    }
    // Blobs of other objects stay, even those a dropped entry needed.
    for kept in [a.result, a.tree, a.child, c_result] {
        assert!(
            matches!(m.blob(&kept), BlobAnswer::Present { .. }),
            "{kept}"
        );
    }
    assert!(hit(&m, &d));
    assert_eq!((m.blob_count(), m.action_count()), (blobs - 2, actions - 3));
}

/// Catches: an eviction that matches a blob by anything but its current location:
/// - one that compares only the sequence number (mutant `o.seq() == x.seq()`): a blob
///   healed into the same sequence number of a later epoch, as a restarted writer
///   numbers its objects, would go with the object it left;
/// - one that also drops the blobs of other unreachable objects (mutant: match on
///   "named or marked");
/// - one that leaves the evicted object's mark (mutant: no clear), so a later
///   `ObjectUnreachable` on it would count no loss.
#[test]
fn evict_keeps_blob_healed_elsewhere() {
    let mut m = meta();
    let (healed, lost, other) = (digest(1), digest(2), digest(3));
    put(&mut m, healed, in_segment(1, 0));
    put(&mut m, lost, in_segment(1, 1));
    put(&mut m, other, in_segment(3, 0));
    let (needs_healed, needs_lost, needs_other) = (digest(10), digest(11), digest(12));
    put_action(&mut m, Role::Daemon, needs_healed, record(healed, &[])).expect("write");
    put(&mut m, digest(13), in_segment(4, 0));
    put_action(
        &mut m,
        Role::Daemon,
        needs_lost,
        record(digest(13), &[lost]),
    )
    .expect("write");
    put_action(&mut m, Role::Daemon, needs_other, record(other, &[])).expect("write");
    mark_missing(&mut m, 1);
    mark(&mut m, 3, UnreachableReason::Corrupt);
    // A restarted writer re-uploads `healed` into object 1 of epoch 2.
    assert_eq!(
        m.execute(ANY_INDEX, Command::AllocEpoch),
        Applied::Epoch(Epoch::new(2))
    );
    let elsewhere = Location {
        store: StoreId::CONFIGURED,
        object: ObjectId::new(Epoch::new(2), 1),
        offset: 0,
    };
    assert_eq!(
        put(&mut m, healed, elsewhere),
        BlobWrite::Healed {
            replaced: in_segment(1, 0)
        }
    );

    let evicted = evict(&mut m, &[object(1)]);

    assert_eq!(
        evicted,
        Evicted {
            blobs: vec![lost],
            actions: vec![needs_lost],
        }
    );
    assert_eq!(
        m.blob(&healed),
        BlobAnswer::Present {
            location: elsewhere,
            staged_on: None,
        }
    );
    assert!(hit(&m, &needs_healed));
    assert_eq!(m.loss_mark(object(1)), None);
    assert_eq!(m.unreachable(object(3)), Some(UnreachableReason::Corrupt));
    assert_eq!(m.blob(&other), BlobAnswer::Unavailable);
    assert_eq!(
        m.action(&needs_other),
        ActionAnswer::Miss(Miss::Unreachable(other))
    );
}

/// Catches: an eviction that leaves `losses` (mutant: no bump), so a reader that
/// remembered "these blobs are held" before it would keep trusting that; one that
/// counts per blob or per object named instead of per object that held a blob; and a
/// retry that counts again.
#[test]
fn evict_raises_losses() {
    let mut m = meta();
    put(&mut m, digest(1), in_segment(1, 0));
    put(&mut m, digest(2), in_segment(1, 1));
    put(&mut m, digest(3), in_segment(2, 0));
    let before = m.losses();
    // Objects 1 (two blobs) and 2 (one) held blobs; object 3 never held one.
    let named = [object(1), object(2), object(3)];
    let evicted = evict(&mut m, &named);
    assert_eq!(evicted.blobs.len(), 3);
    assert_eq!(m.losses(), before + 2);
    assert_eq!(evict(&mut m, &named), Evicted::default());
    assert_eq!(m.losses(), before + 2, "a retry counted again");
}

/// Catches: an eviction that removes a staged object's blobs (mutant: the staged guard
/// removed). Its only copy is its block on the tier, which the store never held, so
/// an evictor or a start-up pass that found no file must not take it away. An object
/// named with it is still evicted.
#[test]
fn evict_skips_staged() {
    let mut m = meta();
    let tier = TierId::new(7);
    assert_eq!(
        m.execute(
            ANY_INDEX,
            Command::AddTier {
                server: ServerId::new(1),
                tier,
            },
        ),
        Applied::Tier(Ok(()))
    );
    let staged = digest(1);
    let applied = m.execute(
        ANY_INDEX,
        Command::PutStaged {
            tier,
            objects: vec![StagedPut {
                object: object(5),
                end: 100,
                seal: false,
                blobs: vec![(staged, 0)],
            }],
        },
    );
    assert!(matches!(applied, Applied::Staged(Ok(_))), "{applied:?}");
    let action = digest(10);
    put_action(&mut m, Role::Daemon, action, record(staged, &[])).expect("write");
    let stored = digest(2);
    put(&mut m, stored, in_segment(2, 0));
    let before = m.losses();

    let evicted = evict(&mut m, &[object(2), object(5)]);

    assert_eq!(
        evicted,
        Evicted {
            blobs: vec![stored],
            actions: vec![],
        }
    );
    assert_eq!(
        m.blob(&staged),
        BlobAnswer::Present {
            location: in_segment(5, 0),
            staged_on: Some(tier),
        }
    );
    assert!(m.staged(object(5)).is_some());
    assert!(hit(&m, &action));
    assert_eq!(m.losses(), before + 1);
}

/// A distinct digest per `n`.
fn numbered(n: u64) -> Digest {
    let mut hash = [0u8; 32];
    hash[..8].copy_from_slice(&n.to_be_bytes());
    Digest::new(DigestFunction::Sha256, hash, n)
}

/// How long one `EvictObjects` takes to apply on a large index: 1 M blobs, 1 000 per
/// object, and 1 M actions, each needing its result and a closure of three blobs
/// spread over the objects. Every apply scans the whole index (and the whole action
/// cache when a blob left) on the log's apply path, so this decides how many objects
/// one command should name. Prints the numbers; asserts only what each apply removed.
///
/// `cargo test -p kbf-meta --test evict --release -- --ignored --nocapture measure`
#[test]
#[ignore = "a measurement: builds a 1 M blob, 1 M action index; run by hand"]
fn measure_evict_apply_on_1m_blobs_and_1m_actions() {
    const BLOBS: u64 = 1_000_000;
    const PER_OBJECT: u64 = 1_000;
    let mut m = meta();
    let started = Instant::now();
    let blobs: Vec<(Digest, Location)> = (0..BLOBS)
        .map(|n| {
            (
                numbered(n),
                in_segment_of(n / PER_OBJECT + 1, n % PER_OBJECT),
            )
        })
        .collect();
    assert!(matches!(
        m.execute(ANY_INDEX, Command::PutBlobs(blobs)),
        Applied::Blobs(Ok(_))
    ));
    for n in 0..BLOBS {
        // Result n; closure n+1, n+333_331, n+666_661: about four objects per action.
        let closure = [1, 333_331, 666_661].map(|k| numbered((n + k) % BLOBS));
        put_action(
            &mut m,
            Role::Daemon,
            numbered(BLOBS + n),
            record(numbered(n), &closure),
        )
        .expect("write");
    }
    println!(
        "built {} blobs, {} actions in {:?}",
        m.blob_count(),
        m.action_count(),
        started.elapsed()
    );
    let objects = BLOBS / PER_OBJECT;
    let mut next = 1;
    for batch in [1, 16, 256] {
        let named: Vec<ObjectId> = (next..next + batch).map(object).collect();
        next += batch;
        let (blobs, actions) = (m.blob_count(), m.action_count());
        let started = Instant::now();
        let evicted = evict(&mut m, &named);
        let took = started.elapsed();
        assert_eq!(u64::try_from(evicted.blobs.len()), Ok(batch * PER_OBJECT));
        println!(
            "evict {batch:>3} objects: {took:?}, removed {} blobs, {} actions (of {blobs}, {actions})",
            evicted.blobs.len(),
            evicted.actions.len()
        );
    }
    // Objects that hold nothing: the index scan alone.
    let none: Vec<ObjectId> = (objects + 1..objects + 257).map(object).collect();
    let started = Instant::now();
    assert_eq!(evict(&mut m, &none), Evicted::default());
    println!("evict 256 objects holding nothing: {:?}", started.elapsed());
}

/// [`in_segment`] for offsets past 255.
fn in_segment_of(segment: u64, n: u64) -> Location {
    Location {
        store: StoreId::CONFIGURED,
        object: object(segment),
        offset: n * 100,
    }
}

/// Catches an eviction that changes anything when it names no object, or only objects
/// that hold nothing (an unbounded scan that "cleans up" on the way).
#[test]
fn evict_of_nothing_held_changes_nothing() {
    let mut m = meta();
    cache_action(&mut m);
    let before = m.clone();
    assert_eq!(evict(&mut m, &[]), Evicted::default());
    assert_eq!(evict(&mut m, &[object(9)]), Evicted::default());
    assert_eq!(m, before);
}
