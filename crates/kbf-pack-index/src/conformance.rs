//! The conformance suite for [`PackIndex`], P1 to P10, generic over a [`PackBackend`] factory
//! so the in-memory index and the Postgres one run the same clauses. A backend's own tests
//! call [`run_all`] or one clause with their factory.
//!
//! Every assertion message starts with its clause and says what a wrong index would do. No
//! clause reads the wall clock: time is the [`ManualClock`] the index is opened with, and
//! waits are joins.

use std::fmt::Debug;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::{
    BlobDigest, Location, MAX_BATCH, NewPack, PackBlob, PackIndex, PackIndexError, PackInfo,
    Recorded, micros_of,
};
use kbf_state::{Clock, ManualClock};

/// Makes indexes for the suite.
pub trait PackBackend {
    /// The index type.
    type Index: PackIndex;

    /// A fresh, empty index whose clock is `clock`.
    fn open_index(&self, clock: Arc<ManualClock>) -> Self::Index;

    /// Another handle on the data `index` holds, with its own connection and the same clock.
    fn reconnect(&self, index: &Self::Index) -> Self::Index;
}

/// Runs P1 to P10 against `backend`.
pub fn run_all<B: PackBackend>(backend: &B) {
    p1_record_and_locate(backend);
    p2_record_is_idempotent(backend);
    p3_conflicting_record_is_refused(backend);
    p4_first_location_is_kept(backend);
    p5_batches_of_a_thousand(backend);
    p6_touch_changes_only_the_touch_time(backend);
    p7_dead_packs_and_unreferenced_keys(backend);
    p8_concurrent_records_all_succeed(backend);
    p9_a_pack_that_cannot_exist_is_refused(backend);
    p10_concurrent_records_of_one_pack_agree(backend);
}

const KEY: &str = "cas/d/n1/1/1";
const OTHER: &str = "cas/f/fe1/1/1";
const THIRD: &str = "cas/f/fe2/1/1";

/// The length of a pack's footer: the bytes after its last blob.
const FOOTER: u64 = 16;

#[track_caller]
fn same<T: PartialEq + Debug>(why: &str, got: T, want: T) {
    assert_eq!(got, want, "{why}");
}

#[track_caller]
fn yes(why: &str, condition: bool) {
    assert!(condition, "{why}");
}

/// The digest of blob number `n`, `size` bytes long. Distinct numbers are distinct hashes.
fn digest(n: u32, size: u64) -> BlobDigest {
    let mut hash = [0_u8; 32];
    hash[..4].copy_from_slice(&n.to_be_bytes());
    BlobDigest { hash, size }
}

fn digests(blobs: &[(u32, u64)]) -> Vec<BlobDigest> {
    blobs.iter().map(|&(n, s)| digest(n, s)).collect()
}

/// `n` digests of blobs of 7 bytes, numbered from 0.
fn many(n: u32) -> Vec<BlobDigest> {
    (0..n).map(|k| digest(k, 7)).collect()
}

fn blob(n: u32, size: u64, offset: u64) -> PackBlob {
    PackBlob {
        digest: digest(n, size),
        offset,
    }
}

/// A pack named `key` written by `writer`, of `blobs` (number and size) laid end to end and
/// a footer.
fn packed(key: &str, writer: &str, blobs: &[(u32, u64)]) -> NewPack {
    let mut offset = 0;
    let mut laid = Vec::new();
    for &(n, size) in blobs {
        laid.push(blob(n, size, offset));
        offset += size;
    }
    NewPack {
        key: key.to_owned(),
        writer: writer.to_owned(),
        size: offset + FOOTER,
        blobs: laid,
    }
}

fn created(indexed: u32) -> Recorded {
    Recorded {
        created: true,
        indexed,
    }
}

fn existing(indexed: u32) -> Recorded {
    Recorded {
        created: false,
        indexed,
    }
}

fn conflict() -> PackIndexError {
    PackIndexError::Conflict {
        key: KEY.to_owned(),
    }
}

/// Where a blob is, as `locate` answers for a blob that has a row.
fn loc(key: &str, offset: u64, length: u64, touched: i64) -> Option<Location> {
    Some(Location {
        pack_key: key.to_owned(),
        offset,
        length,
        last_touch_us: touched,
    })
}

/// A recorded pack, as `pack` answers: its size, when it was recorded, how many blobs it
/// holds and how many of them the index points at it.
fn info_of(key: &str, writer: &str, size: u64, recorded: (i64, u32, u32)) -> Option<PackInfo> {
    Some(PackInfo {
        key: key.to_owned(),
        writer: writer.to_owned(),
        size,
        created_us: recorded.0,
        blob_count: recorded.1,
        live_blobs: recorded.2,
    })
}

fn at(clock: &ManualClock) -> i64 {
    micros_of(clock.now())
}

fn record(index: &dyn PackIndex, pack: &NewPack) -> Recorded {
    index.record_pack(pack).expect("recorded")
}

fn refused(index: &dyn PackIndex, pack: &NewPack) -> PackIndexError {
    index.record_pack(pack).expect_err("a refusal")
}

fn info(index: &dyn PackIndex, key: &str) -> Option<PackInfo> {
    index.pack(key).expect("read")
}

fn locate(index: &dyn PackIndex, digests: &[BlobDigest]) -> Vec<Option<Location>> {
    index.locate(digests).expect("located")
}

fn missing(index: &dyn PackIndex, digests: &[BlobDigest]) -> Vec<BlobDigest> {
    index.find_missing(digests).expect("checked")
}

fn touch(index: &dyn PackIndex, digests: &[BlobDigest]) -> usize {
    index.touch(digests).expect("touched")
}

fn dead(index: &dyn PackIndex, grace: Duration, limit: usize) -> Vec<PackInfo> {
    index.dead_packs(grace, limit).expect("dead packs")
}

fn orphans(index: &dyn PackIndex, keys: &[String]) -> Vec<String> {
    index.unreferenced_keys(keys).expect("checked")
}

fn keys_of(packs: &[PackInfo]) -> Vec<&str> {
    packs.iter().map(|p| p.key.as_str()).collect()
}

fn strings(keys: &[&str]) -> Vec<String> {
    keys.iter().map(|k| (*k).to_owned()).collect()
}

/// A pack with `edit` applied to its blob list.
fn with_blobs(pack: &NewPack, edit: impl FnOnce(&mut Vec<PackBlob>)) -> NewPack {
    let mut blobs = pack.blobs.clone();
    edit(&mut blobs);
    NewPack {
        blobs,
        ..pack.clone()
    }
}

/// Everything the index says about the packs `KEY` and `OTHER` and the blobs `all`.
#[derive(Debug, PartialEq)]
struct Snapshot {
    located: Vec<Option<Location>>,
    first: Option<PackInfo>,
    second: Option<PackInfo>,
}

fn snapshot(index: &dyn PackIndex, all: &[BlobDigest]) -> Snapshot {
    Snapshot {
        located: locate(index, all),
        first: info(index, KEY),
        second: info(index, OTHER),
    }
}

/// P1: a recorded pack is found. Every blob is located at the offset its pack lists, with its
/// length and the time it was recorded; a blob nobody recorded is missing (a repeated digest
/// is answered each time); an empty question has an empty answer; and the pack reads back as
/// recorded, with the clock's reading as its creation time.
pub fn p1_record_and_locate<B: PackBackend>(backend: &B) {
    let clock = Arc::new(ManualClock::default());
    let index = backend.open_index(Arc::clone(&clock));
    clock.advance(Duration::from_secs(5));
    let now = at(&clock);
    let first = packed(KEY, "n1", &[(1, 10), (2, 20), (3, 30)]);
    same(
        "P1 record: a new pack indexes each of its blobs and no more",
        record(&index, &first),
        created(3),
    );
    let asked = digests(&[(3, 30), (9, 9), (1, 10), (3, 30)]);
    let want = vec![
        loc(KEY, 30, 30, now),
        None,
        loc(KEY, 0, 10, now),
        loc(KEY, 30, 30, now),
    ];
    same(
        "P1 locate: each blob is where its pack lists it and a missing one is None",
        locate(&index, &asked),
        want,
    );
    same(
        "P1 locate: a call that asks for nothing is answered with nothing",
        locate(&index, &[]),
        Vec::new(),
    );
    same(
        "P1 pack: the pack reads back as it was recorded, at the clock's reading",
        info(&index, KEY),
        info_of(KEY, "n1", first.size, (now, 3, 3)),
    );
    same(
        "P1 pack: a key nobody recorded has no pack",
        info(&index, OTHER),
        None,
    );
}

/// P2: the same pack recorded twice changes nothing. The second answer says the pack was not
/// new, and the pack, the locations of its blobs and their touch times are as the first
/// record left them although time passed; it holds for a pack with a blob that a pack
/// recorded before it already had, whose own row count is lower than its blob count, and for a
/// pack whose blobs are listed in another order.
pub fn p2_record_is_idempotent<B: PackBackend>(backend: &B) {
    let clock = Arc::new(ManualClock::default());
    let index = backend.open_index(Arc::clone(&clock));
    let first = packed(KEY, "n1", &[(1, 10), (2, 20)]);
    let second = packed(OTHER, "n2", &[(2, 20), (3, 30)]);
    record(&index, &first);
    same(
        "P2 record: the blob a pack recorded earlier has is not indexed twice",
        record(&index, &second),
        created(1),
    );
    let all = digests(&[(1, 10), (2, 20), (3, 30)]);
    let before = snapshot(&index, &all);
    clock.advance(Duration::from_secs(60));
    same(
        "P2 record: a pack recorded again is answered as not new, with its rows",
        record(&index, &first),
        existing(2),
    );
    same(
        "P2 record: a pack with a blob held elsewhere is answered as not new too",
        record(&index, &second),
        existing(1),
    );
    same(
        "P2 record: a pack with its blobs listed in another order is the same pack",
        record(&index, &with_blobs(&first, |b| b.reverse())),
        existing(2),
    );
    same(
        "P2 record: recording a pack again changed a row, a time or a location",
        snapshot(&index, &all),
        before,
    );
}

/// P3: a second record of a key with other contents is refused and changes nothing. The
/// other contents are another writer, another size, a blob at another offset, a blob fewer, a
/// blob more and a blob that the pack indexed swapped for another, and none of them leaves a
/// row behind. (A blob that was indexed in another pack at the first record is not compared.)
pub fn p3_conflicting_record_is_refused<B: PackBackend>(backend: &B) {
    let index = backend.open_index(Arc::new(ManualClock::default()));
    let first = packed(KEY, "n1", &[(1, 10), (2, 20), (3, 30)]);
    record(&index, &first);
    let all = digests(&[(1, 10), (2, 20), (3, 30), (4, 40), (4, 5)]);
    let before = snapshot(&index, &all);
    let variants = [
        NewPack {
            writer: "n2".to_owned(),
            ..first.clone()
        },
        NewPack {
            size: first.size + 1,
            ..first.clone()
        },
        with_blobs(&first, |b| {
            b[0].offset = 20;
            b[1].offset = 0;
        }),
        with_blobs(&first, |b| b.truncate(2)),
        with_blobs(&first, |b| b.push(blob(4, 5, 60))),
        with_blobs(&first, |b| b[2] = blob(4, 40, 30)),
    ];
    for variant in &variants {
        same(
            "P3 conflict: a record of a key with other contents must be refused",
            refused(&index, variant),
            conflict(),
        );
    }
    same(
        "P3 conflict: a refused record left a pack, a row or a time behind",
        snapshot(&index, &all),
        before,
    );
}

/// P4: the first location of a blob is kept. A blob that a pack recorded earlier has stays
/// where it was, and the later pack's own row count is the blobs it was first for; a blob of
/// the same hash and another size is another blob.
pub fn p4_first_location_is_kept<B: PackBackend>(backend: &B) {
    let index = backend.open_index(Arc::new(ManualClock::default()));
    let first = packed(KEY, "n1", &[(1, 10), (2, 20)]);
    let second = packed(OTHER, "n2", &[(2, 20), (3, 30), (2, 21)]);
    record(&index, &first);
    same(
        "P4 first: a blob already indexed is kept, the new ones are indexed",
        record(&index, &second),
        created(2),
    );
    let asked = digests(&[(2, 20), (3, 30), (2, 21)]);
    let want = vec![
        loc(KEY, 10, 20, 0),
        loc(OTHER, 20, 30, 0),
        loc(OTHER, 50, 21, 0),
    ];
    same(
        "P4 first: the blob keeps the pack it was first recorded in",
        locate(&index, &asked),
        want,
    );
    same(
        "P4 first: the first pack still counts the blob among its rows",
        info(&index, KEY),
        info_of(KEY, "n1", first.size, (0, 2, 2)),
    );
    same(
        "P4 first: the later pack counts only the rows it was first for",
        info(&index, OTHER),
        info_of(OTHER, "n2", second.size, (0, 3, 2)),
    );
}

/// P5: a call takes up to a thousand digests. A thousand digests of which every other one
/// has a row are answered in order (the missing ones, a repeated missing digest each time,
/// and the located ones), a call with more than a thousand is refused, and a call that asks
/// for nothing touches nothing.
pub fn p5_batches_of_a_thousand<B: PackBackend>(backend: &B) {
    let index = backend.open_index(Arc::new(ManualClock::default()));
    let held: Vec<(u32, u64)> = (0..500).map(|n| (n * 2, 7)).collect();
    record(&index, &packed(KEY, "n1", &held));
    let asked = many(1000);
    let odd: Vec<BlobDigest> = (0..500).map(|n| digest(n * 2 + 1, 7)).collect();
    same(
        "P5 missing: the digests without a row, in the order they were asked",
        missing(&index, &asked),
        odd,
    );
    let located = locate(&index, &asked);
    let present: Vec<bool> = located.iter().map(Option::is_some).collect();
    let evens: Vec<bool> = (0..1000).map(|n| n % 2 == 0).collect();
    same(
        "P5 locate: every digest with a row is located and no other",
        present,
        evens,
    );
    let twice = [digest(9999, 7), digest(9999, 7)];
    same(
        "P5 missing: a digest asked twice that has no row is reported twice",
        missing(&index, &twice),
        twice.to_vec(),
    );
    same(
        "P5 limit: a call takes a thousand digests, no more",
        MAX_BATCH,
        1000,
    );
    let over = many(1001);
    let refusal = PackIndexError::TooMany {
        given: 1001,
        max: MAX_BATCH,
    };
    same(
        "P5 limit: locate refuses more than a thousand digests",
        index.locate(&over).unwrap_err(),
        refusal.clone(),
    );
    same(
        "P5 limit: find_missing refuses more than a thousand digests",
        index.find_missing(&over).unwrap_err(),
        refusal.clone(),
    );
    same(
        "P5 limit: touch refuses more than a thousand digests",
        index.touch(&over).unwrap_err(),
        refusal.clone(),
    );
    let keys: Vec<String> = (0..1001).map(|n| format!("cas/x/{n}")).collect();
    same(
        "P5 limit: unreferenced_keys refuses more than a thousand keys",
        index.unreferenced_keys(&keys).unwrap_err(),
        refusal,
    );
    same(
        "P5 touch: a call that asks for nothing touches nothing at all",
        touch(&index, &[]),
        0,
    );
}

/// P6: a touch sets the touch time of the blobs that have a row, and changes nothing else. It
/// reports how many blobs it touched (a repeated digest or one without a row counts for
/// nothing), the blobs it was not asked about keep their time, and the pack, the offsets and
/// the lengths are as they were.
pub fn p6_touch_changes_only_the_touch_time<B: PackBackend>(backend: &B) {
    let clock = Arc::new(ManualClock::default());
    let index = backend.open_index(Arc::clone(&clock));
    let first = packed(KEY, "n1", &[(1, 10), (2, 20), (3, 30)]);
    record(&index, &first);
    let pack_before = info(&index, KEY);
    clock.advance(Duration::from_secs(10));
    let later = at(&clock);
    let asked = digests(&[(1, 10), (1, 10), (2, 20), (9, 9)]);
    same(
        "P6 touch: it counts the blobs it touched, once each, and no other",
        touch(&index, &asked),
        2,
    );
    let all = digests(&[(1, 10), (2, 20), (3, 30), (9, 9)]);
    let want = vec![
        loc(KEY, 0, 10, later),
        loc(KEY, 10, 20, later),
        loc(KEY, 30, 30, 0),
        None,
    ];
    same(
        "P6 touch: only the touched blobs have a new time, nothing else moved",
        locate(&index, &all),
        want,
    );
    same(
        "P6 touch: the pack row changed when its blobs were touched",
        info(&index, KEY),
        pack_before,
    );
}

/// P7: the garbage-collection queries are read-only and exact. A pack no blob points at is dead
/// once it is older than the grace (not at the grace, a microsecond after it), and the dead
/// are listed oldest first up to the limit; a pack some blob points at is never dead; and the
/// keys among a listing that no pack is recorded under are reported in the order given.
pub fn p7_dead_packs_and_unreferenced_keys<B: PackBackend>(backend: &B) {
    let clock = Arc::new(ManualClock::default());
    let index = backend.open_index(Arc::clone(&clock));
    let hour = Duration::from_secs(3600);
    record(&index, &packed(KEY, "n1", &[(1, 10), (2, 20)]));
    record(&index, &packed(OTHER, "n2", &[(1, 10)]));
    same(
        "P7 dead: a pack inside the grace is not dead",
        dead(&index, hour, 10),
        Vec::new(),
    );
    clock.advance(hour);
    same(
        "P7 dead: a pack as old as the grace is not yet older than it",
        dead(&index, hour, 10),
        Vec::new(),
    );
    clock.advance(Duration::from_micros(1));
    let listed = dead(&index, hour, 10);
    same(
        "P7 dead: a pack no blob points at is dead once it is older than the grace",
        keys_of(&listed),
        vec![OTHER],
    );
    record(&index, &packed(THIRD, "n3", &[]));
    same(
        "P7 dead: a pack recorded just now is inside the grace",
        keys_of(&dead(&index, hour, 10)),
        vec![OTHER],
    );
    clock.advance(hour * 2);
    let listed = dead(&index, hour, 10);
    same(
        "P7 dead: the dead are listed oldest first and a live pack never is",
        keys_of(&listed),
        vec![OTHER, THIRD],
    );
    let one = dead(&index, hour, 1);
    same(
        "P7 dead: the list is cut at the limit, keeping the oldest",
        keys_of(&one),
        vec![OTHER],
    );
    let listing = strings(&[KEY, "cas/x/orphan", OTHER, THIRD, "cas/x/orphan"]);
    same(
        "P7 unreferenced: the keys no pack is recorded under, in the order given",
        orphans(&index, &listing),
        strings(&["cas/x/orphan", "cas/x/orphan"]),
    );
    same(
        "P7 unreferenced: a listing of nothing has nothing unreferenced",
        orphans(&index, &[]),
        Vec::new(),
    );
}

/// Records twenty packs of 25 blobs on `index`, the blobs of pack `k` numbered from `25 * k`,
/// each pack listing its blobs in reverse when `reversed`. The packs of another writer name
/// the same blobs, so the writers race for every row.
fn record_twenty(index: &dyn PackIndex, who: u32, reversed: bool) -> Vec<Recorded> {
    let mut done = Vec::new();
    for k in 0..20 {
        let mut blobs: Vec<(u32, u64)> = (k * 25..k * 25 + 25).map(|n| (n, 3)).collect();
        if reversed {
            blobs.reverse();
        }
        let key = format!("cas/f/t{who}/1/{k}");
        done.push(record(index, &packed(&key, "fe", &blobs)));
    }
    done
}

/// P8: two connections record different packs at once, naming the same blobs in opposite
/// order, and both succeed: no recording waits for another in a cycle. Every blob ends with
/// one row, in a pack that holds it, and the packs' row counts add up to the blobs.
pub fn p8_concurrent_records_all_succeed<B: PackBackend>(backend: &B) {
    let index = backend.open_index(Arc::new(ManualClock::default()));
    let other = backend.reconnect(&index);
    let (first, second) = thread::scope(|scope| {
        let a = scope.spawn(|| record_twenty(&index, 0, false));
        let b = scope.spawn(|| record_twenty(&other, 1, true));
        (
            a.join().expect("the first writer did not panic"),
            b.join().expect("the second writer did not panic"),
        )
    });
    let both = first.iter().chain(&second);
    yes(
        "P8 concurrent: a pack of either writer was not recorded as new",
        both.clone().all(|r| r.created),
    );
    let rows: u32 = both.map(|r| r.indexed).sum();
    same(
        "P8 concurrent: each blob was indexed by exactly one of the packs naming it",
        rows,
        500,
    );
    let mut live = 0;
    for who in 0..2 {
        for k in 0..20 {
            let key = format!("cas/f/t{who}/1/{k}");
            live += info(&index, &key).map_or(0, |p| p.live_blobs);
        }
    }
    same(
        "P8 concurrent: the packs' row counts do not add up to the blobs",
        live,
        500,
    );
    let all: Vec<BlobDigest> = (0..500).map(|n| digest(n, 3)).collect();
    let located = locate(&index, &all);
    yes(
        "P8 concurrent: a blob named by a recorded pack has no row",
        located.iter().all(Option::is_some),
    );
    for (n, found) in located.iter().enumerate() {
        let key = found.as_ref().map_or("", |l| l.pack_key.as_str());
        yes(
            "P8 concurrent: a blob is indexed in a pack that does not hold it",
            key.ends_with(&format!("/{}", n / 25)),
        );
    }
}

/// P9: a pack that cannot exist is refused and leaves nothing behind: one with no key, one
/// longer than a `bigint` holds, one that lists a digest twice, one with a blob past its end,
/// one whose blob would end past the largest offset and one whose blobs
/// overlap.
pub fn p9_a_pack_that_cannot_exist_is_refused<B: PackBackend>(backend: &B) {
    let index = backend.open_index(Arc::new(ManualClock::default()));
    let base = packed(KEY, "n1", &[(1, 10)]);
    let no_key = packed("", "n1", &[(1, 10)]);
    let huge = NewPack {
        size: u64::MAX,
        ..base.clone()
    };
    let twice = packed(KEY, "n1", &[(1, 10), (1, 10)]);
    let past = NewPack {
        size: 5,
        ..base.clone()
    };
    let wrapped = with_blobs(&base, |b| b[0].offset = u64::MAX);
    let shared = with_blobs(&packed(KEY, "n1", &[(1, 10), (2, 20)]), |b| b[1].offset = 5);
    let cases = [
        (no_key, "the pack key is empty"),
        (huge, "the pack is longer than the index can hold"),
        (twice, "a digest appears twice in the pack"),
        (past, "a blob reaches past the end of the pack"),
        (wrapped, "a blob reaches past the end of the pack"),
        (shared, "two blobs of the pack overlap"),
    ];
    for (pack, why) in cases {
        same(
            "P9 invalid: a pack that cannot exist must be refused",
            refused(&index, &pack),
            PackIndexError::Invalid(why),
        );
        same(
            "P9 invalid: a refused pack left a row behind",
            info(&index, &pack.key),
            None,
        );
    }
    same(
        "P9 invalid: a refused pack indexed a blob",
        locate(&index, &digests(&[(1, 10)])),
        vec![None],
    );
}

/// P10: two connections record the same pack at once, as a daemon's retry does while its first
/// attempt is still in flight. Both succeed, exactly one answers that the pack was new, both
/// report the rows the pack was recorded with, and the index holds one pack and one row per
/// blob.
pub fn p10_concurrent_records_of_one_pack_agree<B: PackBackend>(backend: &B) {
    let index = backend.open_index(Arc::new(ManualClock::default()));
    let other = backend.reconnect(&index);
    let pack = packed(KEY, "n1", &[(1, 10), (2, 20), (3, 30)]);
    let (first, second) = thread::scope(|scope| {
        let a = scope.spawn(|| record(&index, &pack));
        let b = scope.spawn(|| record(&other, &pack));
        (
            a.join().expect("the first writer did not panic"),
            b.join().expect("the second writer did not panic"),
        )
    });
    let new = [first, second].iter().filter(|r| r.created).count();
    same(
        "P10 concurrent: exactly one record of the pack is new",
        new,
        1,
    );
    same(
        "P10 concurrent: both records report the rows the pack was recorded with",
        (first.indexed, second.indexed),
        (3, 3),
    );
    same(
        "P10 concurrent: the pack is recorded once, with its three rows",
        info(&index, KEY),
        info_of(KEY, "n1", pack.size, (0, 3, 3)),
    );
    let all = digests(&[(1, 10), (2, 20), (3, 30)]);
    let want = vec![loc(KEY, 0, 10, 0), loc(KEY, 10, 20, 0), loc(KEY, 30, 30, 0)];
    same(
        "P10 concurrent: each blob has one row, in the pack, at its offset",
        locate(&index, &all),
        want,
    );
}
