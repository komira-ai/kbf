//! A pack the store answers absent for: the reader has the index forget it whole only once a
//! second look, some time after the first absence, confirms it, and only a pack old enough; a
//! store that fails to answer (a 503, a reset connection, a timeout) is never a look at all.
//! Time is the rig's manual clock, which the index and the loss guard read.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

use kbf_objstore::ObjectStore;
use kbf_pack::{
    CONFIRM_AFTER, CONFIRM_WITHIN, FORGET_INTERVAL, Fetch, Loss, LossClock, LossGuard, LossRules,
    MAX_NOTED, MIN_AGE, PackerConfig, ReadError, ReaderConfig,
};
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_pack_index::{BlobDigest, NewPack, PackBlob, PackIndex};
use kbf_state::{Clock as _, ManualClock};

use crate::reader::{found, packed, row};
use crate::support::{Rig, blob, keys, pack_all, pack_key};

/// A blob the store did not give, its pack not forgotten.
const LOST: Fetch = Fetch::Lost { forgotten: false };
/// A blob the store did not give, its pack forgotten.
const FORGOTTEN: Fetch = Fetch::Lost { forgotten: true };

const MICRO: Duration = Duration::from_micros(1);

/// Asserts the loop a lost pack takes, by pack and not by blob: the first read that finds packs
/// absent (one gone, one whose range starts past its end, one that ends inside a blob) forgets
/// nothing and asks no length, and the index still says the blobs are held; a read
/// [`CONFIRM_AFTER`] later asks each pack's length once and has the index forget each whole: its
/// blobs, the one not read too, are reported missing, its row is gone, and the next read finds
/// no row; a blob packed again, as a client's upload packs it, is found.
#[tokio::test]
async fn a_pack_absent_twice_some_time_apart_is_forgotten_whole() {
    let gone = blob("a blob whose pack was deleted");
    let unread = blob("a blob of the same pack that nobody read");
    let past = blob("a blob that starts past the end of its object");
    let cut = blob("a blob that its object ends in the middle of");
    let rig = packed(&[gone.clone(), unread.clone()]).await;
    rig.store.delete(&keys().key(0)).await.unwrap();
    rig.store.seed("cas/x/past", b"0123456789").await;
    rig.store.seed("cas/x/cut", b"0123456789").await;
    rig.index
        .record_pack(&row("cas/x/past", 100, &past, 50))
        .unwrap();
    rig.index
        .record_pack(&row("cas/x/cut", 100, &cut, 5))
        .unwrap();
    rig.wait(MIN_AGE);
    let reader = rig.reader(None, ReaderConfig::default());
    let asked = [gone.digest, past.digest, cut.digest];
    let all = [gone.digest, unread.digest, past.digest, cut.digest];

    let first = reader.read(&asked).await.unwrap();
    let believed = rig.index.find_missing(&all).unwrap();
    let first_lengths = rig.store.lens();
    rig.wait(CONFIRM_AFTER);
    let second = reader.read(&asked).await.unwrap();

    assert_eq!(first, [LOST, LOST, LOST]);
    assert!(believed.is_empty(), "{believed:?}");
    assert!(first_lengths.is_empty(), "{first_lengths:?}");
    assert_eq!(second, [FORGOTTEN, FORGOTTEN, FORGOTTEN]);
    let mut lengths = rig.store.lens();
    lengths.sort();
    assert_eq!(lengths, [&*pack_key(0), "cas/x/cut", "cas/x/past"]);
    assert_eq!(rig.index.find_missing(&all).unwrap(), all);
    for key in [&*pack_key(0), "cas/x/cut", "cas/x/past"] {
        assert!(rig.index.pack(key).unwrap().is_none(), "{key}");
    }
    let again = reader.read(&asked).await.unwrap();
    assert_eq!(again, [Fetch::Missing, Fetch::Missing, Fetch::Missing]);

    let packer = rig.packer(PackerConfig::default());
    pack_all(&packer, std::slice::from_ref(&gone)).await;
    assert_eq!(reader.read(&[gone.digest]).await.unwrap(), [found(&gone)]);
}

/// Asserts that a lost pack is forgotten alone: the blobs of another pack read in the same calls
/// are found and keep their rows, and only the lost pack's length is asked.
#[tokio::test]
async fn forgetting_a_lost_pack_keeps_the_rows_of_the_others() {
    let gone = blob("a blob whose pack was deleted");
    let kept = blob("a blob whose pack is held");
    let rig = Rig::new();
    let packer = rig.packer(PackerConfig::default());
    pack_all(&packer, std::slice::from_ref(&gone)).await;
    pack_all(&packer, std::slice::from_ref(&kept)).await;
    rig.store.delete(&keys().key(0)).await.unwrap();
    rig.wait(MIN_AGE);
    let reader = rig.reader(None, ReaderConfig::default());

    let first = reader.read(&[gone.digest, kept.digest]).await.unwrap();
    rig.wait(CONFIRM_AFTER);
    let second = reader.read(&[gone.digest, kept.digest]).await.unwrap();

    assert_eq!(first, [LOST, found(&kept)]);
    assert_eq!(second, [FORGOTTEN, found(&kept)]);
    let missing = rig.index.find_missing(&[gone.digest, kept.digest]).unwrap();
    assert_eq!(missing, [gone.digest]);
    assert_eq!(rig.store.lens(), [pack_key(0)]);
}

/// Asserts the bounds of the wait for the second look: an absence a microsecond under
/// [`CONFIRM_AFTER`] after the first asks no length and forgets nothing; one at exactly
/// [`CONFIRM_AFTER`] forgets the pack.
#[tokio::test]
async fn an_absence_sooner_than_the_wait_forgets_nothing() {
    let gone = blob("a blob whose pack was deleted");
    let rig = packed(std::slice::from_ref(&gone)).await;
    rig.store.delete(&keys().key(0)).await.unwrap();
    rig.wait(MIN_AGE);
    let reader = rig.reader(None, ReaderConfig::default());

    let first = reader.read(&[gone.digest]).await.unwrap();
    rig.wait(CONFIRM_AFTER - MICRO);
    let early = reader.read(&[gone.digest]).await.unwrap();
    let early_lengths = rig.store.lens();
    let kept = rig.index.find_missing(&[gone.digest]).unwrap();
    rig.wait(MICRO);
    let due = reader.read(&[gone.digest]).await.unwrap();

    assert_eq!(first, [LOST]);
    assert_eq!(early, [LOST]);
    assert!(early_lengths.is_empty(), "{early_lengths:?}");
    assert!(kept.is_empty(), "{kept:?}");
    assert_eq!(due, [FORGOTTEN]);
    assert_eq!(rig.store.lens(), [pack_key(0)]);
}

/// Asserts that a pack younger than [`MIN_AGE`] is not forgotten even when the second look
/// confirms its absence (the length is asked, the rows stay), and that once it is old enough,
/// still within [`CONFIRM_WITHIN`] of the first absence, it is.
#[tokio::test]
async fn a_young_pack_is_not_forgotten_until_it_is_old_enough() {
    let gone = blob("a blob whose pack was deleted right after it was written");
    let rig = packed(std::slice::from_ref(&gone)).await;
    rig.store.delete(&keys().key(0)).await.unwrap();
    let reader = rig.reader(None, ReaderConfig::default());

    let first = reader.read(&[gone.digest]).await.unwrap();
    rig.wait(CONFIRM_AFTER);
    let young = reader.read(&[gone.digest]).await.unwrap();
    let young_lengths = rig.store.lens();
    let kept = rig.index.find_missing(&[gone.digest]).unwrap();
    let young_pack = rig.index.pack(&pack_key(0)).unwrap();
    rig.wait(MIN_AGE - CONFIRM_AFTER);
    let old = reader.read(&[gone.digest]).await.unwrap();

    assert_eq!(first, [LOST]);
    assert_eq!(young, [LOST]);
    assert_eq!(young_lengths, [pack_key(0)]);
    assert!(kept.is_empty(), "{kept:?}");
    assert!(young_pack.is_some());
    assert_eq!(old, [FORGOTTEN]);
    assert_eq!(
        rig.index.find_missing(&[gone.digest]).unwrap(),
        [gone.digest]
    );
}

/// Asserts that a first absence more than [`CONFIRM_WITHIN`] ago confirms nothing: the absence
/// after it is a new first look (no length asked), and the one [`CONFIRM_AFTER`] after that
/// forgets the pack.
#[tokio::test]
async fn a_first_absence_too_long_ago_is_a_new_first_look() {
    let gone = blob("a blob whose pack was deleted");
    let rig = packed(std::slice::from_ref(&gone)).await;
    rig.store.delete(&keys().key(0)).await.unwrap();
    rig.wait(MIN_AGE);
    let reader = rig.reader(None, ReaderConfig::default());

    let first = reader.read(&[gone.digest]).await.unwrap();
    rig.wait(CONFIRM_WITHIN + MICRO);
    let stale = reader.read(&[gone.digest]).await.unwrap();
    let stale_lengths = rig.store.lens();
    rig.wait(CONFIRM_AFTER);
    let due = reader.read(&[gone.digest]).await.unwrap();

    assert_eq!(first, [LOST]);
    assert_eq!(stale, [LOST]);
    assert!(stale_lengths.is_empty(), "{stale_lengths:?}");
    assert_eq!(due, [FORGOTTEN]);
}

/// Asserts the case of a store whose volume servers restart, its other replica holding every
/// object: reads and lengths answer "not found" for a few minutes and then the bytes again, and
/// nothing is forgotten (no length is even asked). The read that gets the bytes clears the absence noted, so an absence
/// [`CONFIRM_AFTER`] later is a new first look; and a second look that finds the object whole
/// keeps the rows and clears the note too.
#[tokio::test]
async fn a_store_that_answers_absent_while_a_replica_restarts_loses_no_pack() {
    let held = blob("a blob whose pack the store holds on its other replica");
    let rig = packed(std::slice::from_ref(&held)).await;
    rig.wait(MIN_AGE);
    let reader = rig.reader(None, ReaderConfig::default());

    // Reads and lengths alike are routed to the replica that is down.
    rig.store.lose_gets(2);
    rig.store.lose_lens(2);
    let restarting = reader.read(&[held.digest]).await.unwrap();
    rig.wait(Duration::from_secs(60));
    let still = reader.read(&[held.digest]).await.unwrap();
    rig.wait(Duration::from_secs(120));
    let back = reader.read(&[held.digest]).await.unwrap();
    rig.wait(CONFIRM_AFTER);
    rig.store.lose_gets(1);
    let next_restart = reader.read(&[held.digest]).await.unwrap();
    let lengths = rig.store.lens();

    assert_eq!(restarting, [LOST]);
    assert_eq!(still, [LOST]);
    assert_eq!(back, [found(&held)]);
    assert_eq!(next_restart, [LOST]);
    assert!(lengths.is_empty(), "{lengths:?}");
    assert!(rig.index.find_missing(&[held.digest]).unwrap().is_empty());
    rig.store.lose_lens(0);

    rig.wait(CONFIRM_AFTER);
    rig.store.lose_gets(1);
    let whole = reader.read(&[held.digest]).await.unwrap();
    rig.wait(CONFIRM_AFTER);
    rig.store.lose_gets(1);
    let after_whole = reader.read(&[held.digest]).await.unwrap();

    assert_eq!(whole, [LOST]);
    assert_eq!(after_whole, [LOST]);
    assert_eq!(
        rig.store.lens(),
        [pack_key(0)],
        "one second look, which found it whole"
    );
    assert!(rig.index.find_missing(&[held.digest]).unwrap().is_empty());
    assert_eq!(reader.read(&[held.digest]).await.unwrap(), [found(&held)]);
}

/// Asserts that a pack cut short is forgotten though the blobs before the cut are read in
/// between: a read that ends before the bytes found absent does not clear the absence, so the
/// blob past the cut, absent again [`CONFIRM_AFTER`] later, has the pack forgotten, the blob
/// before the cut with it.
#[tokio::test]
async fn reads_before_the_cut_of_a_short_pack_do_not_clear_its_absence() {
    let before = blob("0123456789");
    let after = blob("a blob the object ends before");
    let rig = Rig::new();
    rig.store.seed("cas/x/cut", b"0123456789").await;
    let pack = NewPack {
        key: "cas/x/cut".to_owned(),
        writer: "w".to_owned(),
        size: 200,
        blobs: vec![
            PackBlob {
                digest: before.digest,
                offset: 0,
            },
            PackBlob {
                digest: after.digest,
                offset: 100,
            },
        ],
    };
    rig.index.record_pack(&pack).unwrap();
    rig.wait(MIN_AGE);
    let reader = rig.reader(None, ReaderConfig::default());

    let first = reader.read(&[after.digest]).await.unwrap();
    let held = reader.read(&[before.digest]).await.unwrap();
    rig.wait(CONFIRM_AFTER);
    let held_again = reader.read(&[before.digest]).await.unwrap();
    let confirmed = reader.read(&[after.digest]).await.unwrap();

    assert_eq!(first, [LOST]);
    assert_eq!(held, [found(&before)]);
    assert_eq!(held_again, [found(&before)]);
    assert_eq!(confirmed, [FORGOTTEN]);
    let all = [before.digest, after.digest];
    assert_eq!(rig.index.find_missing(&all).unwrap(), all);
}

/// Asserts that a store that fails to answer forgets nothing, even for a pack that is gone: a
/// read answered 503 or cut by a reset connection fails the read and is no first look, at any
/// time; a second look answered 503 or a timeout keeps the rows; and only then, with the store
/// answering, does a second look forget the pack.
#[tokio::test]
async fn a_store_that_fails_to_answer_forgets_nothing() {
    let gone = blob("a blob whose pack was deleted");
    let rig = packed(std::slice::from_ref(&gone)).await;
    rig.store.delete(&keys().key(0)).await.unwrap();
    rig.wait(MIN_AGE);
    let reader = rig.reader(None, ReaderConfig::default());
    let failed = |answer: Result<Vec<Fetch>, ReadError>| {
        assert!(matches!(answer, Err(ReadError::Store { .. })), "{answer:?}");
    };

    for _ in 0..2 {
        rig.store.fail_gets(1);
        failed(reader.read(&[gone.digest]).await);
        rig.store.reset_gets(1);
        failed(reader.read(&[gone.digest]).await);
        rig.wait(CONFIRM_AFTER);
    }
    let first = reader.read(&[gone.digest]).await.unwrap();
    let unasked = rig.store.lens();
    rig.wait(CONFIRM_AFTER);
    rig.store.fail_lens(1);
    let slow_down = reader.read(&[gone.digest]).await.unwrap();
    rig.store.time_out_lens(1);
    let timed_out = reader.read(&[gone.digest]).await.unwrap();
    let kept = rig.index.find_missing(&[gone.digest]).unwrap();
    let confirmed = reader.read(&[gone.digest]).await.unwrap();

    assert_eq!(first, [LOST], "the first absence, after the failures");
    assert!(unasked.is_empty(), "{unasked:?}");
    assert_eq!(slow_down, [LOST]);
    assert_eq!(timed_out, [LOST]);
    assert!(kept.is_empty(), "{kept:?}");
    assert_eq!(confirmed, [FORGOTTEN]);
    assert_eq!(rig.store.lens().len(), 3);
}

/// Asserts that a confirmed loss whose forget the index fails keeps the rows, and that the next
/// absence, the index answering again, forgets the pack.
#[tokio::test]
async fn a_forget_the_index_fails_keeps_the_rows() {
    let gone = blob("a blob whose pack was deleted");
    let rig = packed(std::slice::from_ref(&gone)).await;
    rig.store.delete(&keys().key(0)).await.unwrap();
    rig.wait(MIN_AGE);
    let reader = rig.reader(None, ReaderConfig::default());
    reader.read(&[gone.digest]).await.unwrap();
    rig.wait(CONFIRM_AFTER);
    rig.index.fail_forget(1);

    let refused = reader.read(&[gone.digest]).await.unwrap();
    let still = rig.index.find_missing(&[gone.digest]).unwrap();
    let forgotten = reader.read(&[gone.digest]).await.unwrap();

    assert_eq!(refused, [LOST]);
    assert!(still.is_empty(), "{still:?}");
    assert_eq!(forgotten, [FORGOTTEN]);
    assert_eq!(
        rig.index.find_missing(&[gone.digest]).unwrap(),
        [gone.digest]
    );
}

/// Asserts that a damaged blob whose second read finds no bytes is lost, not damaged, and that
/// its pack's absence is noted as any other first absence: no length is asked, and its row is
/// kept.
#[tokio::test]
async fn a_damaged_blob_lost_on_its_second_read_is_a_first_absence() {
    let genuine = blob("0123456789");
    let rig = Rig::new();
    rig.store.seed("cas/x/honest", b"0123456789").await;
    rig.index
        .record_pack(&row("cas/x/honest", 10, &genuine, 0))
        .unwrap();
    rig.store.garble_gets(1);
    rig.store.lose_gets_after(1, 1);
    let reader = rig.reader(None, ReaderConfig::default());

    let got = reader.read(&[genuine.digest]).await.unwrap();

    assert_eq!(got, [LOST]);
    assert!(rig.store.lens().is_empty());
    assert!(rig.index.locate(&[genuine.digest]).unwrap()[0].is_some());
}

/// Records the one-blob pack `key` in `index`.
fn record(index: &dyn PackIndex, key: &str) {
    let pack = NewPack {
        key: key.to_owned(),
        writer: "w".to_owned(),
        size: 10,
        blobs: vec![PackBlob {
            digest: BlobDigest {
                hash: [7; 32],
                size: 10,
            },
            offset: 0,
        }],
    };
    index.record_pack(&pack).unwrap();
}

/// An index on `clock` with the one-blob pack `key` recorded.
fn index_with(clock: &Arc<ManualClock>, key: &str) -> Arc<dyn PackIndex> {
    let index: Arc<dyn PackIndex> = Arc::new(MemoryPackIndex::new(Arc::clone(clock) as _));
    record(&*index, key);
    index
}

/// A guard with the default rules but `min_age` and `max_noted`, reading `clock`.
fn guard_with(clock: &Arc<ManualClock>, min_age: Duration, max_noted: usize) -> LossGuard {
    let clock = Arc::clone(clock);
    let rules = LossRules {
        min_age,
        max_noted,
        ..LossRules::default()
    };
    LossGuard::new(
        rules,
        Arc::new(move || SystemTime::UNIX_EPOCH + clock.now().as_duration()),
    )
}

/// A guard with the default rules but `min_age`, reading `clock`.
fn guard(clock: &Arc<ManualClock>, min_age: Duration) -> LossGuard {
    guard_with(clock, min_age, MAX_NOTED)
}

type Look = std::future::Ready<Result<Option<u64>, &'static str>>;

/// A second look that counts its calls in `looks` and answers `length`.
fn looking<'a>(
    looks: &'a AtomicU32,
    length: Result<Option<u64>, &'static str>,
) -> impl FnOnce() -> Look + 'a {
    move || {
        looks.fetch_add(1, Ordering::SeqCst);
        std::future::ready(length)
    }
}

const KEY: &str = "cas/l/k";

/// Asserts that a key forgotten is not forgotten again within [`FORGET_INTERVAL`], with no look
/// made, though it was recorded again and is absent again; and that at the interval exactly it
/// is a first absence again and can be forgotten again.
#[tokio::test]
async fn a_key_forgotten_is_not_forgotten_again_within_the_interval() {
    let clock = Arc::new(ManualClock::default());
    let index = index_with(&clock, KEY);
    let guard = guard(&clock, Duration::ZERO);
    let looks = AtomicU32::new(0);

    let first = guard
        .absent(&index, KEY, 10, looking(&looks, Ok(None)))
        .await;
    clock.advance(CONFIRM_AFTER);
    let forgot = guard
        .absent(&index, KEY, 10, looking(&looks, Ok(None)))
        .await;
    record(&*index, KEY);
    clock.advance(FORGET_INTERVAL - MICRO);
    let recent = guard
        .absent(&index, KEY, 10, looking(&looks, Ok(None)))
        .await;
    clock.advance(MICRO);
    let noted = guard
        .absent(&index, KEY, 10, looking(&looks, Ok(None)))
        .await;
    clock.advance(CONFIRM_AFTER);
    let forgot_again = guard
        .absent(&index, KEY, 10, looking(&looks, Ok(None)))
        .await;

    assert_eq!(first, Loss::Noted);
    assert_eq!(forgot, Loss::Forgotten { rows: 1 });
    assert!(forgot.forgotten());
    assert_eq!(recent, Loss::Recent);
    assert!(!recent.forgotten());
    assert_eq!(noted, Loss::Noted);
    assert_eq!(forgot_again, Loss::Forgotten { rows: 1 });
    assert_eq!(looks.load(Ordering::SeqCst), 2);
}

/// Asserts what each answer of the second look comes to: an object at least `end` long is held
/// and clears the note (the next absence is a first one), and an `end` of 0 takes any object for
/// held; one shorter than `end` is confirmed and forgotten; a look with no answer is
/// unconfirmed, says why, and keeps the note (the next absence looks again at once); a key the
/// index never recorded is confirmed, not recorded, and clears the note.
#[tokio::test]
async fn the_second_look_decides() {
    let clock = Arc::new(ManualClock::default());
    let index = index_with(&clock, KEY);
    let guard = guard(&clock, Duration::ZERO);
    let looks = AtomicU32::new(0);
    // A first absence, and the second look `CONFIRM_AFTER` later answering `length`.
    let twice = |key: &'static str, end: u64, length| {
        let (guard, index, clock, looks) = (&guard, &index, &clock, &looks);
        async move {
            let first = guard
                .absent(index, key, end, looking(looks, Ok(None)))
                .await;
            assert_eq!(first, Loss::Noted, "{key} {end} {length:?}");
            clock.advance(CONFIRM_AFTER);
            guard.absent(index, key, end, looking(looks, length)).await
        }
    };

    let whole = twice(KEY, 10, Ok(Some(10))).await;
    let any = twice(KEY, 0, Ok(Some(0))).await;
    let short = twice(KEY, 10, Ok(Some(9))).await;
    record(&*index, KEY);
    clock.advance(FORGET_INTERVAL);
    let unanswered = twice(KEY, 10, Err("store answered 503 SlowDown")).await;
    let kept_note = guard
        .absent(&index, KEY, 10, looking(&looks, Ok(None)))
        .await;
    clock.advance(FORGET_INTERVAL);
    let unrecorded = twice("cas/none", 10, Ok(None)).await;
    let cleared = guard
        .absent(&index, "cas/none", 10, looking(&looks, Ok(None)))
        .await;

    assert_eq!(whole, Loss::Held);
    assert_eq!(any, Loss::Held);
    assert_eq!(short, Loss::Forgotten { rows: 1 });
    assert_eq!(
        unanswered,
        Loss::Unconfirmed("store answered 503 SlowDown".to_owned())
    );
    assert_eq!(kept_note, Loss::Forgotten { rows: 1 });
    assert_eq!(unrecorded, Loss::NotRecorded);
    assert_eq!(cleared, Loss::Noted);
    assert_eq!(looks.load(Ordering::SeqCst), 6);
}

/// Asserts that a pack the index says is younger than the guard's least age is not forgotten,
/// and that its note stays, so a later look forgets it once it is old enough; and that an index
/// that fails the forget keeps the note too, saying why.
#[tokio::test]
async fn a_young_pack_or_a_failed_forget_keeps_the_note() {
    let clock = Arc::new(ManualClock::default());
    let index = index_with(&clock, KEY);
    let guard = guard(&clock, MIN_AGE);
    let looks = AtomicU32::new(0);

    guard
        .absent(&index, KEY, 10, looking(&looks, Ok(None)))
        .await;
    clock.advance(CONFIRM_AFTER);
    let young = guard
        .absent(&index, KEY, 10, looking(&looks, Ok(None)))
        .await;
    clock.advance(MIN_AGE - CONFIRM_AFTER);
    let old = guard
        .absent(&index, KEY, 10, looking(&looks, Ok(None)))
        .await;

    let rig = Rig::new();
    let flaky: Arc<dyn PackIndex> = rig.index.clone();
    record(&*flaky, KEY);
    rig.index.fail_forget(1);
    let failing = LossGuard::new(LossRules::default(), rig.loss_clock());
    rig.wait(MIN_AGE);
    failing
        .absent(&flaky, KEY, 10, looking(&looks, Ok(None)))
        .await;
    rig.wait(CONFIRM_AFTER);
    let refused = failing
        .absent(&flaky, KEY, 10, looking(&looks, Ok(None)))
        .await;
    let retried = failing
        .absent(&flaky, KEY, 10, looking(&looks, Ok(None)))
        .await;

    assert_eq!(young, Loss::Young);
    assert_eq!(old, Loss::Forgotten { rows: 1 });
    assert!(
        matches!(refused, Loss::IndexFailed(ref why) if !why.is_empty()),
        "{refused:?}"
    );
    assert_eq!(retried, Loss::Forgotten { rows: 1 });
}

/// Asserts that a clock that went back makes an absence too soon, never a confirmation.
#[tokio::test]
async fn a_clock_that_went_back_confirms_nothing() {
    let clock = Arc::new(ManualClock::default());
    let at = |time: Duration| -> LossClock { Arc::new(move || SystemTime::UNIX_EPOCH + time) };
    let guard = LossGuard::new(LossRules::default(), at(CONFIRM_WITHIN));
    let index = index_with(&clock, KEY);
    let looks = AtomicU32::new(0);

    let first = guard
        .absent(&index, KEY, 10, looking(&looks, Ok(None)))
        .await;
    guard.set_clock(at(CONFIRM_WITHIN - CONFIRM_AFTER));
    let back = guard
        .absent(&index, KEY, 10, looking(&looks, Ok(None)))
        .await;

    assert_eq!(first, Loss::Noted);
    assert_eq!(back, Loss::TooSoon);
    assert_eq!(looks.load(Ordering::SeqCst), 0);
}

/// Asserts that the guard notes at most its `max_noted` packs: past it the oldest note goes (its
/// next absence is a first one again), and the others stay; and that a read of a pack never
/// noted, or of one with no notes at all, clears nothing.
#[tokio::test]
async fn the_notes_are_bounded_and_the_oldest_goes_first() {
    let clock = Arc::new(ManualClock::default());
    let index = index_with(&clock, KEY);
    let guard = guard_with(&clock, Duration::ZERO, 3);
    let looks = AtomicU32::new(0);
    guard.held("cas/n/0", u64::MAX);
    for n in 0..4 {
        let key = format!("cas/n/{n}");
        guard
            .absent(&index, &key, 10, looking(&looks, Ok(None)))
            .await;
        clock.advance(MICRO);
    }
    guard.held("cas/never-noted", u64::MAX);
    clock.advance(CONFIRM_AFTER);

    let oldest = guard
        .absent(&index, "cas/n/0", 10, looking(&looks, Ok(None)))
        .await;
    let next = guard
        .absent(&index, "cas/n/2", 10, looking(&looks, Ok(None)))
        .await;

    assert_eq!(oldest, Loss::Noted);
    assert_eq!(next, Loss::NotRecorded);
    assert_eq!(looks.load(Ordering::SeqCst), 1);
}

/// Asserts that a read clears an absence only when it got the bytes through the furthest end
/// found absent: one byte short keeps the note (the next absence is a second look), the end
/// exactly clears it (the next absence is a first one again).
#[tokio::test]
async fn a_read_clears_an_absence_only_through_its_end() {
    let clock = Arc::new(ManualClock::default());
    let index = index_with(&clock, KEY);
    let guard = guard(&clock, Duration::ZERO);
    let looks = AtomicU32::new(0);

    guard
        .absent(&index, KEY, 10, looking(&looks, Ok(None)))
        .await;
    guard
        .absent(&index, KEY, 20, looking(&looks, Ok(None)))
        .await;
    guard.held(KEY, 19);
    clock.advance(CONFIRM_AFTER);
    let kept = guard
        .absent(&index, KEY, 5, looking(&looks, Ok(Some(25))))
        .await;
    guard
        .absent(&index, KEY, 10, looking(&looks, Ok(None)))
        .await;
    guard.held(KEY, 10);
    clock.advance(CONFIRM_AFTER);
    let cleared = guard
        .absent(&index, KEY, 10, looking(&looks, Ok(None)))
        .await;

    assert_eq!(kept, Loss::Held, "the note stayed: a second look was made");
    assert_eq!(cleared, Loss::Noted);
    assert_eq!(looks.load(Ordering::SeqCst), 1);
}

/// Asserts that a guard told to note no pack notes one at a time: each new pack's absence
/// replaces the last, and a pack noted can still be confirmed.
#[tokio::test]
async fn a_guard_with_no_room_for_notes_keeps_one() {
    let clock = Arc::new(ManualClock::default());
    let index = index_with(&clock, KEY);
    let guard = guard_with(&clock, Duration::ZERO, 0);
    let looks = AtomicU32::new(0);

    let other = guard
        .absent(&index, "cas/n/x", 10, looking(&looks, Ok(None)))
        .await;
    let first = guard
        .absent(&index, KEY, 10, looking(&looks, Ok(None)))
        .await;
    clock.advance(CONFIRM_AFTER);
    let forgot = guard
        .absent(&index, KEY, 10, looking(&looks, Ok(None)))
        .await;
    let replaced = guard
        .absent(&index, "cas/n/x", 10, looking(&looks, Ok(None)))
        .await;

    assert_eq!((other, first), (Loss::Noted, Loss::Noted));
    assert_eq!(forgot, Loss::Forgotten { rows: 1 });
    assert_eq!(replaced, Loss::Noted, "its note went for the next pack's");
}

/// Asserts the default rules and the default guard: the published constants, field by field.
#[test]
fn the_default_rules_are_the_published_constants() {
    let rules = LossRules::default();
    let guard = LossGuard::default();

    assert_eq!(
        rules,
        LossRules {
            confirm_after: CONFIRM_AFTER,
            confirm_within: CONFIRM_WITHIN,
            min_age: MIN_AGE,
            forget_interval: FORGET_INTERVAL,
            max_noted: MAX_NOTED,
        }
    );
    assert_eq!(guard.rules(), rules);
    assert!(format!("{guard:?}").starts_with("LossGuard { rules: LossRules {"));
}
