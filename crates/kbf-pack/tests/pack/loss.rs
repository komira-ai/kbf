//! The witness of lost packs: when a loss is confirmed, what it remembers, and its bound.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use kbf_pack::{CONFIRM_AFTER, Loss, LossRule, LossWitness, WITNESS_ENTRIES};
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_pack_index::{BlobDigest, NewPack, PackBlob, PackIndex};
use kbf_state::ManualClock;

/// A witness on a clock the test moves, an index whose packs are all old enough, and the clock.
fn rig() -> (LossWitness, Arc<dyn PackIndex>, Arc<Mutex<SystemTime>>) {
    let now = Arc::new(Mutex::new(
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000),
    ));
    let read = Arc::clone(&now);
    let clock = Arc::new(move || *read.lock().unwrap_or_else(PoisonError::into_inner));
    let rule = LossRule {
        confirm_after: CONFIRM_AFTER,
        min_age: Duration::ZERO,
    };
    let index: Arc<dyn PackIndex> =
        Arc::new(MemoryPackIndex::new(Arc::new(ManualClock::default())));
    (LossWitness::new(rule, clock), index, now)
}

fn wait(now: &Mutex<SystemTime>, by: Duration) {
    *now.lock().unwrap_or_else(PoisonError::into_inner) += by;
}

/// Records a pack `key` of one blob of 10 bytes, a blob of its own.
fn record(index: &Arc<dyn PackIndex>, key: &str) {
    let mut hash = [0_u8; 32];
    for (at, byte) in key.bytes().enumerate() {
        hash[at % 32] ^= byte.rotate_left(u32::try_from(at % 8).unwrap());
    }
    let blob = PackBlob {
        digest: BlobDigest { hash, size: 10 },
        offset: 0,
    };
    let pack = NewPack {
        key: key.to_owned(),
        writer: "w".to_owned(),
        size: 10,
        blobs: vec![blob],
    };
    index.record_pack(&pack).unwrap();
}

/// Asserts the rule step by step: a held length is `Held`; the first look at a loss is
/// `Unconfirmed`; a look the confirming time later forgets the pack (`Forgotten` with its rows);
/// a pack no longer recorded is `Young` (kept, nothing to forget); and a clock that goes back
/// confirms nothing.
#[tokio::test]
async fn a_loss_is_forgotten_at_the_look_that_confirms_it() {
    let (witness, index, now) = rig();
    record(&index, "cas/x/p");

    let held = witness
        .judge(&index, "cas/x/p", Some(10), 10)
        .await
        .unwrap();
    let first = witness.judge(&index, "cas/x/p", None, 10).await.unwrap();
    wait(&now, CONFIRM_AFTER);
    let forgotten = witness.judge(&index, "cas/x/p", Some(4), 10).await.unwrap();
    witness.judge(&index, "cas/x/p", None, 10).await.unwrap();
    wait(&now, CONFIRM_AFTER);
    let gone = witness.judge(&index, "cas/x/p", None, 10).await.unwrap();

    assert_eq!(held, Loss::Held);
    assert_eq!(first, Loss::Unconfirmed);
    assert_eq!(forgotten, Loss::Forgotten { rows: 1 });
    assert!(forgotten.forgotten() && !first.forgotten());
    assert_eq!(gone, Loss::Young, "nothing recorded under the key is kept");

    let (back, index, now) = rig();
    record(&index, "cas/x/q");
    back.judge(&index, "cas/x/q", None, 10).await.unwrap();
    *now.lock().unwrap_or_else(PoisonError::into_inner) -= Duration::from_secs(60);
    let earlier = back.judge(&index, "cas/x/q", None, 10).await.unwrap();
    assert_eq!(earlier, Loss::Unconfirmed);
}

/// Asserts that a witness remembers at most [`WITNESS_ENTRIES`] first looks, the oldest going
/// first: the first pack seen needs a new first look, the last one is confirmed.
#[tokio::test]
async fn the_first_looks_are_bounded_oldest_first() {
    let (witness, index, now) = rig();
    for n in 0..=WITNESS_ENTRIES {
        witness
            .judge(&index, &format!("cas/x/{n}"), None, 1)
            .await
            .unwrap();
    }
    record(&index, "cas/x/0");
    let last = format!("cas/x/{WITNESS_ENTRIES}");
    record(&index, &last);
    wait(&now, CONFIRM_AFTER);

    let oldest = witness.judge(&index, "cas/x/0", None, 1).await.unwrap();
    let newest = witness.judge(&index, &last, None, 1).await.unwrap();

    assert_eq!(oldest, Loss::Unconfirmed);
    assert_eq!(newest, Loss::Forgotten { rows: 1 });
}

/// Asserts that a witness's rule and clock can be set after it is made, and that it shows its
/// rule: a rule that confirms at once forgets at the second look on the same clock.
#[tokio::test]
async fn a_witness_takes_a_new_rule_and_clock() {
    let witness = LossWitness::default();
    let index: Arc<dyn PackIndex> =
        Arc::new(MemoryPackIndex::new(Arc::new(ManualClock::default())));
    record(&index, "cas/x/p");
    witness.set_rule(LossRule {
        confirm_after: Duration::ZERO,
        min_age: Duration::ZERO,
    });
    witness.set_clock(Arc::new(|| SystemTime::UNIX_EPOCH));

    witness.judge(&index, "cas/x/p", None, 10).await.unwrap();
    let second = witness.judge(&index, "cas/x/p", None, 10).await.unwrap();

    assert_eq!(second, Loss::Forgotten { rows: 1 });
    assert_eq!(LossRule::default().confirm_after, CONFIRM_AFTER);
    let shown = format!("{witness:?}");
    assert!(shown.starts_with("LossWitness { rule: LossRule"), "{shown}");
}
