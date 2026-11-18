//! The reader: blobs by digest through the index, the store and the cache.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use kbf_objstore::ObjectStore;
use kbf_pack::{
    Blob, CONFIRM_AFTER, Fetch, LossRule, LossWitness, MIN_AGE, PackerConfig, ReadCache, ReadError,
    Reader, ReaderConfig,
};
use kbf_pack_index::{MAX_BATCH, NewPack, PackBlob, PackIndex, PackIndexError};

use crate::support::{
    FaultyStore, Rig, blob, cache_dir, digests, filler, keys, limits, pack_all, pack_key,
};

fn found(blob: &Blob) -> Fetch {
    Fetch::Found(blob.bytes.clone())
}

fn expected(blobs: &[Blob]) -> Vec<Fetch> {
    blobs.iter().map(found).collect()
}

/// A reader config with this gap and this touch batch.
fn config(gap: u64, touch_batch: usize) -> ReaderConfig {
    ReaderConfig {
        gap,
        touch_batch,
        ..ReaderConfig::default()
    }
}

/// An index row for a pack `key` of `size` bytes that holds `blob` at `offset`, whatever the
/// store holds under `key`.
fn row(key: &str, size: u64, blob: &Blob, offset: u64) -> NewPack {
    let placed = PackBlob {
        digest: blob.digest,
        offset,
    };
    NewPack {
        key: key.to_owned(),
        writer: "w".to_owned(),
        size,
        blobs: vec![placed],
    }
}

/// A rig with `blobs` packed into one pack by a default packer.
async fn packed(blobs: &[Blob]) -> Rig {
    let rig = Rig::new();
    let packer = rig.packer(PackerConfig::default());
    pack_all(&packer, blobs).await;
    rig
}

/// A reader over `rig` whose losses are judged by the default rule on a clock the test moves,
/// and that clock.
fn witnessed(rig: &Rig) -> (Reader<FaultyStore>, Arc<Mutex<SystemTime>>) {
    let now = Arc::new(Mutex::new(SystemTime::UNIX_EPOCH));
    let read = Arc::clone(&now);
    let clock = Arc::new(move || *read.lock().unwrap_or_else(PoisonError::into_inner));
    let witness = Arc::new(LossWitness::new(LossRule::default(), clock));
    let reader = rig
        .reader(None, ReaderConfig::default())
        .with_witness(witness);
    (reader, now)
}

/// Moves the witness's clock on by `by`.
fn wait(now: &Mutex<SystemTime>, by: Duration) {
    *now.lock().unwrap_or_else(PoisonError::into_inner) += by;
}

/// Moves both clocks on far enough for a loss seen now to be confirmed by the next look: the
/// witness's by [`CONFIRM_AFTER`], the index's by [`MIN_AGE`].
fn later(rig: &Rig, now: &Mutex<SystemTime>) {
    wait(now, CONFIRM_AFTER);
    rig.index.advance(MIN_AGE);
}

const KEPT: Fetch = Fetch::Lost { forgotten: false };
const FORGOTTEN: Fetch = Fetch::Lost { forgotten: true };

/// A cache of 1 MiB in a directory of its own.
fn cache_in(dir: &kbf_testkit::dir::TestDir) -> Arc<ReadCache> {
    Arc::new(ReadCache::open(dir.path(), 1 << 20).unwrap())
}

/// Asserts that the blobs asked for come back in the order asked, and that blobs that lie
/// together in a pack are read with one request for the whole range.
#[tokio::test]
async fn blobs_of_one_pack_are_read_with_one_request() {
    let blobs = [blob("alpha"), blob("bravo"), blob("charlie")];
    let rig = packed(&blobs).await;
    let reader = rig.reader(None, ReaderConfig::default());

    let got = reader.read(&digests(&blobs)).await.unwrap();

    assert_eq!(got, expected(&blobs));
    let total: u64 = blobs.iter().map(|b| b.digest.size).sum();
    assert_eq!(rig.store.ranges(), [(pack_key(0), 0, total)]);
}

/// Asserts that two blobs of a pack that lie further apart than the gap are two requests,
/// that the bytes between them are not asked for, and that with a gap that spans them they are
/// one request.
#[tokio::test]
async fn blobs_far_apart_in_a_pack_are_two_requests() {
    let blobs = [filler(100, 1), filler(200_000, 2), filler(100, 3)];
    let ends = [blobs[0].digest, blobs[2].digest];
    let both = [found(&blobs[0]), found(&blobs[2])];

    let rig = packed(&blobs).await;
    let reader = rig.reader(None, ReaderConfig::default());
    assert_eq!(reader.read(&ends).await.unwrap(), both);
    let mut asked = rig.store.ranges();
    asked.sort();
    let apart = [(pack_key(0), 0, 100), (pack_key(0), 200_100, 100)];
    assert_eq!(asked, apart);

    let rig = packed(&blobs).await;
    let reader = rig.reader(None, config(200_000, MAX_BATCH));
    assert_eq!(reader.read(&ends).await.unwrap(), both);
    assert_eq!(rig.store.ranges(), [(pack_key(0), 0, 200_200)]);
}

/// Asserts that blobs of different packs are read with a request for each pack.
#[tokio::test]
async fn blobs_of_different_packs_are_read_with_a_request_each() {
    let rig = Rig::new();
    let packer = rig.packer(limits(PackerConfig::default().pack_bytes, 1));
    let blobs = [blob("alpha"), blob("bravo"), blob("charlie")];
    pack_all(&packer, &blobs).await;
    let reader = rig.reader(None, ReaderConfig::default());

    let got = reader.read(&digests(&blobs)).await.unwrap();

    assert_eq!(got, expected(&blobs));
    let mut packs: Vec<String> = rig.store.ranges().into_iter().map(|r| r.0).collect();
    packs.sort();
    assert_eq!(packs, [pack_key(0), pack_key(1), pack_key(2)]);
}

/// Asserts that a digest asked twice is read once and answered in each place it was asked.
#[tokio::test]
async fn a_digest_asked_twice_is_read_once() {
    let blobs = [blob("alpha"), blob("bravo")];
    let rig = packed(&blobs).await;
    let reader = rig.reader(None, ReaderConfig::default());
    let asked = [blobs[1].digest, blobs[0].digest, blobs[1].digest];

    let got = reader.read(&asked).await.unwrap();

    let order = [1, 0, 1];
    let want: Vec<Fetch> = order.iter().map(|&i| found(&blobs[i])).collect();
    assert_eq!(got, want);
    assert_eq!(rig.store.ranges().len(), 1);
    assert_eq!(rig.index.locate_batches(), [2]);
}

/// Asserts that a digest the index has no row for is missing, and that no request is made
/// for it.
#[tokio::test]
async fn a_digest_the_index_lacks_is_missing() {
    let rig = Rig::new();
    let reader = rig.reader(None, ReaderConfig::default());

    let got = reader.read(&[blob("nobody").digest]).await.unwrap();

    assert_eq!(got, [Fetch::Missing]);
    assert!(rig.store.ranges().is_empty());
}

/// Asserts that an empty blob is found without a request, and that asking for nothing is
/// answering nothing.
#[tokio::test]
async fn an_empty_blob_is_found_without_a_request() {
    let blobs = [blob(""), blob("other")];
    let rig = packed(&blobs).await;
    let reader = rig.reader(None, ReaderConfig::default());

    let got = reader.read(&[blobs[0].digest]).await.unwrap();

    assert_eq!(got, [found(&blobs[0])]);
    assert!(rig.store.ranges().is_empty());
    assert!(reader.read(&[]).await.unwrap().is_empty());
}

/// Asserts that a row whose pack the store does not hold, whose range starts past the end of
/// the object, or whose range the object cuts short, is lost, not an error; that the first read
/// that finds it so only remembers the loss (each pack asked for its length, the rows kept); that
/// a read a confirming time later, of packs old enough, has the index forget each pack, so the
/// index lists the blobs as missing and the next read finds no row; and that a blob packed again,
/// as a client's upload after the loss packs it, is found.
#[tokio::test]
async fn bytes_the_store_does_not_have_are_lost_and_their_packs_forgotten() {
    let gone = blob("a blob whose pack was deleted");
    let past = blob("a blob that starts past the end of its object");
    let cut = blob("a blob that its object ends in the middle of");
    let rig = packed(std::slice::from_ref(&gone)).await;
    rig.store.delete(&keys().key(0)).await.unwrap();
    rig.store.seed("cas/x/past", b"0123456789").await;
    rig.store.seed("cas/x/cut", b"0123456789").await;
    rig.index
        .record_pack(&row("cas/x/past", 100, &past, 50))
        .unwrap();
    rig.index
        .record_pack(&row("cas/x/cut", 100, &cut, 5))
        .unwrap();
    let (reader, now) = witnessed(&rig);
    let asked = [gone.digest, past.digest, cut.digest];

    let first = reader.read(&asked).await.unwrap();
    let kept = rig.index.find_missing(&asked).unwrap();
    later(&rig, &now);
    let confirmed = reader.read(&asked).await.unwrap();

    assert_eq!(first, [KEPT, KEPT, KEPT]);
    assert!(kept.is_empty(), "the first look forgets nothing");
    assert_eq!(confirmed, [FORGOTTEN, FORGOTTEN, FORGOTTEN]);
    let mut asked_lengths = rig.store.lens();
    asked_lengths.sort();
    let key = pack_key(0);
    let each_twice = [
        key.as_str(),
        &key,
        "cas/x/cut",
        "cas/x/cut",
        "cas/x/past",
        "cas/x/past",
    ];
    assert_eq!(asked_lengths, each_twice);
    assert_eq!(rig.index.find_missing(&asked).unwrap(), asked);
    assert!(rig.index.pack("cas/x/cut").unwrap().is_none());
    let again = reader.read(&asked).await.unwrap();
    assert_eq!(again, [Fetch::Missing, Fetch::Missing, Fetch::Missing]);

    let packer = rig.packer(PackerConfig::default());
    pack_all(&packer, std::slice::from_ref(&gone)).await;
    let uploaded = reader.read(&[gone.digest]).await.unwrap();
    assert_eq!(uploaded, [found(&gone)]);
}

/// Asserts that a lost pack is forgotten alone: the blobs of another pack read in the same calls
/// are found and keep their rows.
#[tokio::test]
async fn forgetting_a_lost_pack_keeps_the_rows_of_the_others() {
    let gone = blob("a blob whose pack was deleted");
    let kept = blob("a blob whose pack is held");
    let rig = Rig::new();
    let packer = rig.packer(PackerConfig::default());
    pack_all(&packer, std::slice::from_ref(&gone)).await;
    pack_all(&packer, std::slice::from_ref(&kept)).await;
    rig.store.delete(&keys().key(0)).await.unwrap();
    let (reader, now) = witnessed(&rig);

    reader.read(&[gone.digest, kept.digest]).await.unwrap();
    later(&rig, &now);
    let got = reader.read(&[gone.digest, kept.digest]).await.unwrap();

    assert_eq!(got, [FORGOTTEN, found(&kept)]);
    let missing = rig.index.find_missing(&[gone.digest, kept.digest]).unwrap();
    assert_eq!(missing, [gone.digest]);
    assert_eq!(rig.store.lens(), [pack_key(0), pack_key(0)]);
}

/// Asserts that a read that finds a pack missing while the store holds it whole (the read raced
/// the pack's write) keeps the rows and is not remembered as a first look: the next read finds
/// the blob, and a real loss after that needs two looks of its own.
#[tokio::test]
async fn a_pack_the_store_holds_whole_after_all_is_kept() {
    let held = blob("a blob whose first read raced its pack");
    let rig = packed(std::slice::from_ref(&held)).await;
    rig.index.advance(MIN_AGE);
    rig.store.lose_gets(1);
    let (reader, now) = witnessed(&rig);

    let first = reader.read(&[held.digest]).await.unwrap();
    let second = reader.read(&[held.digest]).await.unwrap();
    rig.store.delete(&keys().key(0)).await.unwrap();
    wait(&now, CONFIRM_AFTER);
    let lost = reader.read(&[held.digest]).await.unwrap();

    assert_eq!(first, [KEPT]);
    assert_eq!(second, [found(&held)]);
    assert_eq!(
        lost,
        [KEPT],
        "the raced read was not taken for a first look"
    );
    assert!(rig.index.find_missing(&[held.digest]).unwrap().is_empty());
}

/// Asserts that a second look sooner than the confirming time does not confirm the loss, and one
/// at that time does: the loss is forgotten only at the look [`CONFIRM_AFTER`] after the first.
#[tokio::test]
async fn a_loss_seen_again_too_soon_is_not_confirmed() {
    let gone = blob("a blob whose pack was deleted");
    let rig = packed(std::slice::from_ref(&gone)).await;
    rig.index.advance(MIN_AGE);
    rig.store.delete(&keys().key(0)).await.unwrap();
    let (reader, now) = witnessed(&rig);

    let first = reader.read(&[gone.digest]).await.unwrap();
    wait(&now, CONFIRM_AFTER - Duration::from_secs(1));
    let soon = reader.read(&[gone.digest]).await.unwrap();
    wait(&now, Duration::from_secs(1));
    let confirmed = reader.read(&[gone.digest]).await.unwrap();

    assert_eq!((first, soon), (vec![KEPT], vec![KEPT]));
    assert_eq!(confirmed, [FORGOTTEN]);
}

/// Asserts that a pack recorded less than [`MIN_AGE`] ago by the index's clock is never
/// forgotten, however far apart its loss is seen, and is forgotten at the look after it reaches
/// that age.
#[tokio::test]
async fn a_young_pack_is_not_forgotten() {
    let gone = blob("a blob whose pack was deleted when new");
    let rig = packed(std::slice::from_ref(&gone)).await;
    rig.store.delete(&keys().key(0)).await.unwrap();
    let (reader, now) = witnessed(&rig);

    reader.read(&[gone.digest]).await.unwrap();
    wait(&now, CONFIRM_AFTER);
    let young = reader.read(&[gone.digest]).await.unwrap();
    rig.index.advance(MIN_AGE - Duration::from_secs(1));
    let younger = reader.read(&[gone.digest]).await.unwrap();
    rig.index.advance(Duration::from_secs(1));
    let old = reader.read(&[gone.digest]).await.unwrap();

    assert_eq!((young, younger), (vec![KEPT], vec![KEPT]));
    assert_eq!(old, [FORGOTTEN]);
}

/// Asserts that a store that fails to answer a read (a 503) forgets nothing, however late the
/// next look: the read is the store's error, no length is asked, and the rows stay.
#[tokio::test]
async fn a_store_that_fails_to_answer_forgets_nothing() {
    let held = blob("a blob whose store is busy");
    let rig = packed(std::slice::from_ref(&held)).await;
    let (reader, now) = witnessed(&rig);

    rig.store.fail_gets(1);
    let first = reader.read(&[held.digest]).await;
    later(&rig, &now);
    rig.store.fail_gets(1);
    let second = reader.read(&[held.digest]).await;

    assert!(matches!(first, Err(ReadError::Store { .. })), "{first:?}");
    assert!(matches!(second, Err(ReadError::Store { .. })), "{second:?}");
    assert!(rig.store.lens().is_empty());
    assert!(rig.index.find_missing(&[held.digest]).unwrap().is_empty());
}

/// Asserts that a lost pack whose length the store does not give is not looked at (no first look
/// is remembered), and that one whose forget the index fails keeps its rows and its first look;
/// the next read after each forgets as the rule says.
#[tokio::test]
async fn a_loss_that_cannot_be_confirmed_or_forgotten_keeps_the_rows() {
    let gone = blob("a blob whose pack was deleted");
    let rig = packed(std::slice::from_ref(&gone)).await;
    rig.index.advance(MIN_AGE);
    rig.store.delete(&keys().key(0)).await.unwrap();
    let (reader, now) = witnessed(&rig);

    rig.store.fail_lens(1);
    let unasked = reader.read(&[gone.digest]).await.unwrap();
    let first = reader.read(&[gone.digest]).await.unwrap();
    wait(&now, CONFIRM_AFTER);
    rig.index.fail_forget(1);
    let refused = reader.read(&[gone.digest]).await.unwrap();
    let still = rig.index.find_missing(&[gone.digest]).unwrap();
    let forgotten = reader.read(&[gone.digest]).await.unwrap();

    assert_eq!(
        (unasked, first, refused),
        (vec![KEPT], vec![KEPT], vec![KEPT])
    );
    assert!(still.is_empty());
    assert_eq!(forgotten, [FORGOTTEN]);
    let missing = rig.index.find_missing(&[gone.digest]).unwrap();
    assert_eq!(missing, [gone.digest]);
}

/// Asserts what forgetting a pack does to a blob a second pack also holds: the index keeps one
/// location (the first pack's), so once the first pack is forgotten the blob is missing although
/// the second pack has it, and a client uploads it again, which packs it anew and is found.
#[tokio::test]
async fn a_blob_a_second_pack_holds_is_uploaded_again_once_its_first_pack_is_forgotten() {
    let both = blob("0123456789");
    let rig = packed(std::slice::from_ref(&both)).await;
    rig.store.seed("cas/x/second", b"0123456789").await;
    let second = rig
        .index
        .record_pack(&row("cas/x/second", 10, &both, 0))
        .unwrap();
    rig.index.advance(MIN_AGE);
    rig.store.delete(&keys().key(0)).await.unwrap();
    let (reader, now) = witnessed(&rig);

    reader.read(&[both.digest]).await.unwrap();
    later(&rig, &now);
    let got = reader.read(&[both.digest]).await.unwrap();

    assert_eq!(second.indexed, 0, "the first location kept the row");
    assert_eq!(got, [FORGOTTEN]);
    let missing = rig.index.find_missing(&[both.digest]).unwrap();
    assert_eq!(missing, [both.digest]);
    let packer = rig.packer(PackerConfig::default());
    pack_all(&packer, std::slice::from_ref(&both)).await;
    assert_eq!(reader.read(&[both.digest]).await.unwrap(), [found(&both)]);
}

/// Asserts that a damaged blob whose second read finds no bytes is lost, not damaged, and its
/// pack is asked after as any lost pack is: here the store holds it whole, so its row is kept.
#[tokio::test]
async fn a_damaged_blob_lost_on_its_second_read_has_its_pack_asked_after() {
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

    assert_eq!(got, [Fetch::Lost { forgotten: false }]);
    assert_eq!(rig.store.lens(), ["cas/x/honest"]);
    assert!(rig.index.locate(&[genuine.digest]).unwrap()[0].is_some());
}

/// Asserts that bytes that are not the digest are refused and not cached, that the blobs read
/// with them are returned, and that only the blobs served are noted for the index.
#[tokio::test]
async fn bytes_that_are_not_the_digest_are_damaged() {
    let good = blob("a blob that is stored as it should be");
    let genuine = blob("0123456789");
    let rig = packed(std::slice::from_ref(&good)).await;
    rig.store.seed("cas/x/forged", b"ABCDEFGHIJ").await;
    let forged_row = row("cas/x/forged", 10, &genuine, 0);
    rig.index.record_pack(&forged_row).unwrap();
    let dir = cache_dir("reader-damaged");
    let cache = cache_in(&dir);
    let reader = rig.reader(Some(Arc::clone(&cache)), ReaderConfig::default());
    let asked = [genuine.digest, good.digest];

    let got = reader.read(&asked).await.unwrap();

    assert_eq!(got, [Fetch::Damaged { dropped: true }, found(&good)]);
    assert_eq!(cache.entries(), 1);
    assert!(cache.get(&genuine.digest).is_none());
    assert_eq!(reader.flush_touches().await.unwrap(), 1);
}

/// Asserts that a store that fails a read fails the call and names the pack.
#[tokio::test]
async fn a_store_that_fails_fails_the_call() {
    let blobs = [blob("alpha")];
    let rig = packed(&blobs).await;
    rig.store.fail_gets(1);
    let reader = rig.reader(None, ReaderConfig::default());

    let error = reader.read(&digests(&blobs)).await.unwrap_err();

    assert!(matches!(error, ReadError::Store { .. }), "{error}");
    assert!(error.to_string().contains(&pack_key(0)), "{error}");
}

/// Asserts that an index that fails a lookup fails the call.
#[tokio::test]
async fn an_index_that_fails_fails_the_call() {
    let blobs = [blob("alpha")];
    let rig = packed(&blobs).await;
    rig.index.fail_locate(1);
    let reader = rig.reader(None, ReaderConfig::default());

    let error = reader.read(&digests(&blobs)).await.unwrap_err();

    assert!(matches!(error, ReadError::Index(_)), "{error}");
    assert!(error.to_string().contains("pack index"), "{error}");
}

/// Asserts that a row that names its pack by a string that is no object key fails the call.
#[tokio::test]
async fn a_pack_key_that_is_no_object_key_fails_the_call() {
    let rig = Rig::new();
    let x = blob("alpha");
    let bad_row = row("bad key", 10, &x, 0);
    rig.index.record_pack(&bad_row).unwrap();
    let reader = rig.reader(None, ReaderConfig::default());

    let error = reader.read(&[x.digest]).await.unwrap_err();

    assert!(matches!(error, ReadError::Key(_)), "{error}");
    assert!(error.to_string().contains("pack key"), "{error}");
}

/// Asserts that a read asks the index in batches of at most `MAX_BATCH` digests, and that
/// 1,001 small blobs of one pack are one request.
#[tokio::test]
async fn a_read_asks_the_index_in_batches() {
    let blobs: Vec<Blob> = (0..1001).map(|i| blob(&format!("blob {i}"))).collect();
    let rig = packed(&blobs).await;
    let reader = rig.reader(None, ReaderConfig::default());

    let got = reader.read(&digests(&blobs)).await.unwrap();

    assert!(got.iter().all(|f| matches!(f, Fetch::Found(_))));
    assert_eq!(rig.index.locate_batches(), [MAX_BATCH, 1]);
    assert_eq!(rig.store.ranges().len(), 1);
}

/// Asserts that blobs read from the store are kept in the cache, and that a second read of
/// them is answered by the cache with no request to the store and no lookup in the index.
#[tokio::test]
async fn blobs_read_from_the_store_are_served_from_the_cache_afterwards() {
    let blobs = [blob("alpha"), blob("bravo")];
    let rig = packed(&blobs).await;
    let dir = cache_dir("reader-cache");
    let cache = cache_in(&dir);
    let reader = rig.reader(Some(Arc::clone(&cache)), ReaderConfig::default());

    let first = reader.read(&digests(&blobs)).await.unwrap();
    let second = reader.read(&digests(&blobs)).await.unwrap();

    assert_eq!(first, expected(&blobs));
    assert_eq!(second, expected(&blobs));
    assert_eq!(cache.entries(), 2);
    assert_eq!(rig.store.ranges().len(), 1);
    assert_eq!(rig.index.locate_batches().len(), 1);
}

/// Asserts that a cached file whose bytes are not the digest is not served: the blob is read
/// from the store again, and the cache holds the right bytes afterwards.
#[tokio::test]
async fn a_corrupt_cache_file_is_refused_and_read_again() {
    let blobs = [blob("alpha"), blob("bravo")];
    let rig = packed(&blobs).await;
    let dir = cache_dir("reader-corrupt");
    let cache = cache_in(&dir);
    let reader = rig.reader(Some(Arc::clone(&cache)), ReaderConfig::default());
    reader.read(&digests(&blobs)).await.unwrap();
    std::fs::write(cache.path_of(&blobs[0].digest), b"AAAAA").unwrap();

    let got = reader.read(&digests(&blobs)).await.unwrap();

    assert_eq!(got, expected(&blobs));
    assert_eq!(rig.store.ranges().len(), 2);
    assert_eq!(cache.get(&blobs[0].digest), Some(blobs[0].bytes.clone()));
}

/// Asserts that a cache that cannot keep a blob does not fail the read.
#[tokio::test]
async fn a_cache_that_cannot_write_does_not_fail_the_read() {
    let blobs = [blob("alpha")];
    let rig = packed(&blobs).await;
    let dir = cache_dir("reader-unwritable");
    let cache = cache_in(&dir);
    std::fs::remove_dir_all(dir.path()).unwrap();
    let reader = rig.reader(Some(Arc::clone(&cache)), ReaderConfig::default());

    let got = reader.read(&digests(&blobs)).await.unwrap();

    assert_eq!(got, expected(&blobs));
    assert_eq!(cache.entries(), 0);
}

/// Asserts that the index is told of the blobs served once as many as the touch batch have
/// been noted, in one batch, and that flushing with nothing noted asks the index nothing.
#[tokio::test]
async fn served_blobs_are_touched_in_batches() {
    let blobs = [blob("alpha"), blob("bravo")];
    let rig = packed(&blobs).await;
    let reader = rig.reader(None, config(64 << 10, 2));

    reader.read(&[blobs[0].digest]).await.unwrap();
    assert!(rig.index.touch_batches().is_empty());
    reader.read(&[blobs[1].digest]).await.unwrap();
    assert_eq!(rig.index.touch_batches(), [2]);

    assert_eq!(reader.flush_touches().await.unwrap(), 0);
    assert_eq!(rig.index.touch_batches(), [2]);
}

/// Asserts that a flush tells the index of every blob served since the last one, a blob read
/// twice once, and that blobs served from the cache count as served: after a first flush, two
/// reads that the cache answers without a request are flushed as the two blobs.
#[tokio::test]
async fn a_flush_touches_each_blob_served() {
    let blobs = [blob("alpha"), blob("bravo")];
    let rig = packed(&blobs).await;
    let dir = cache_dir("reader-touch");
    let reader = rig.reader(Some(cache_in(&dir)), ReaderConfig::default());
    reader.read(&digests(&blobs)).await.unwrap();
    assert!(rig.index.touch_batches().is_empty());
    assert_eq!(reader.flush_touches().await.unwrap(), 2);

    reader.read(&digests(&blobs)).await.unwrap();
    reader.read(&digests(&blobs)).await.unwrap();
    assert_eq!(rig.store.ranges().len(), 1);
    assert_eq!(reader.flush_touches().await.unwrap(), 2);

    assert_eq!(rig.index.touch_batches(), [2, 2]);
}

/// Asserts that an index that fails a touch does not fail the read that triggered it.
#[tokio::test]
async fn a_failed_touch_does_not_fail_the_read() {
    let blobs = [blob("alpha")];
    let rig = packed(&blobs).await;
    let reader = rig.reader(None, config(64 << 10, 1));
    rig.index.fail_touch(1);

    let got = reader.read(&digests(&blobs)).await.unwrap();

    assert_eq!(got, expected(&blobs));
    assert_eq!(rig.index.touch_batches(), [1]);
}

/// Asserts that a flush the index fails is an error, and that the blobs of the failed batch
/// are not noted again.
#[tokio::test]
async fn a_failed_flush_reports_the_index_error() {
    let blobs = [blob("alpha")];
    let rig = packed(&blobs).await;
    let reader = rig.reader(None, ReaderConfig::default());
    reader.read(&digests(&blobs)).await.unwrap();
    rig.index.fail_touch(1);

    let error = reader.flush_touches().await.unwrap_err();

    assert_eq!(error, PackIndexError::NotMigrated);
    assert_eq!(reader.flush_touches().await.unwrap(), 0);
}

/// Asserts that a record whose bytes are not its digest has its location dropped from the
/// index: the first read is `Damaged` and says it dropped, the index then has no row for the
/// digest, and a second read answers `Missing` rather than `Damaged`.
#[tokio::test]
async fn a_damaged_record_drops_its_location() {
    let genuine = blob("0123456789");
    let rig = Rig::new();
    rig.store.seed("cas/x/forged", b"ABCDEFGHIJ").await;
    rig.index
        .record_pack(&row("cas/x/forged", 10, &genuine, 0))
        .unwrap();
    let reader = rig.reader(None, ReaderConfig::default());

    let first = reader.read(&[genuine.digest]).await.unwrap();
    let second = reader.read(&[genuine.digest]).await.unwrap();

    assert_eq!(first, [Fetch::Damaged { dropped: true }]);
    assert_eq!(rig.index.locate(&[genuine.digest]).unwrap(), [None]);
    assert_eq!(second, [Fetch::Missing]);
}

/// Asserts that a record the store answers garbled once is read again before its location is
/// dropped: the second read has the digest's bytes, so the blob is found, its row stays, and
/// the next read finds it where it was.
#[tokio::test]
async fn a_record_garbled_once_is_read_again_and_its_location_kept() {
    let genuine = blob("0123456789");
    let rig = Rig::new();
    rig.store.seed("cas/x/honest", b"0123456789").await;
    rig.index
        .record_pack(&row("cas/x/honest", 10, &genuine, 0))
        .unwrap();
    rig.store.garble_gets(1);
    let reader = rig.reader(None, ReaderConfig::default());

    let first = reader.read(&[genuine.digest]).await.unwrap();

    assert_eq!(
        first,
        [Fetch::Found(bytes::Bytes::from_static(b"0123456789"))]
    );
    let located = rig.index.locate(&[genuine.digest]).unwrap();
    assert_eq!(
        located[0].as_ref().map(|at| at.pack_key.as_str()),
        Some("cas/x/honest")
    );
    let reads = rig.store.ranges().len();
    assert_eq!(reads, 2, "the record was read twice");
}

/// Asserts that a damaged record whose second read the store fails keeps its location: nothing
/// is dropped on a read that did not see the bytes, and the answer stays `Damaged`.
#[tokio::test]
async fn a_damaged_record_whose_second_read_fails_keeps_its_location() {
    let genuine = blob("0123456789");
    let rig = Rig::new();
    rig.store.seed("cas/x/honest", b"0123456789").await;
    rig.index
        .record_pack(&row("cas/x/honest", 10, &genuine, 0))
        .unwrap();
    rig.store.garble_gets(1);
    rig.store.fail_gets_after(1, 1);
    let reader = rig.reader(None, ReaderConfig::default());

    let first = reader.read(&[genuine.digest]).await.unwrap();

    assert_eq!(first, [Fetch::Damaged { dropped: false }]);
    assert!(rig.index.locate(&[genuine.digest]).unwrap()[0].is_some());
}

/// Asserts that a drop the index does not take leaves the read's answer `Damaged` (not
/// dropped) and the row where it was, so the next read finds the damage again.
#[tokio::test]
async fn a_drop_the_index_refuses_leaves_the_row() {
    let genuine = blob("0123456789");
    let rig = Rig::new();
    rig.store.seed("cas/x/forged", b"ABCDEFGHIJ").await;
    rig.index
        .record_pack(&row("cas/x/forged", 10, &genuine, 0))
        .unwrap();
    rig.index.fail_drop(1);
    let reader = rig.reader(None, ReaderConfig::default());

    let first = reader.read(&[genuine.digest]).await.unwrap();
    let second = reader.read(&[genuine.digest]).await.unwrap();

    assert_eq!(first, [Fetch::Damaged { dropped: false }]);
    assert_eq!(second, [Fetch::Damaged { dropped: true }]);
}
