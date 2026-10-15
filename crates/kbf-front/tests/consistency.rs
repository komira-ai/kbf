//! The start-up consistency pass over a file store (`Cache::consistency_pass`): which
//! files it keeps and deletes, which index entries it forgets and when it refuses to,
//! what it reports as held, and its pace.

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use kbf_front::{
    Cache, CacheError, MemoryMetaLog, MetaLog, MetaLogError, PASS_OPENS_PER_SEC, PassReport,
    VerifiedBlob,
};
use kbf_meta::{
    ActionAnswer, ActionRecord, Applied, Closure, Collected, Command, Epoch, MetaState, Miss,
    ObjectId, Retention, Role, ServerId, StagedPut, TierId,
};
use kbf_objstore::{FsStore, KeyPrefix, ObjectKey, ObjectStore};
use kbf_types::{Digest, FarmTime};

/// One metadata log shared by the caches of successive "processes".
#[derive(Clone)]
struct Log(Arc<MemoryMetaLog>);

impl Log {
    fn new() -> Self {
        Self(Arc::new(MemoryMetaLog::new(Retention::default())))
    }

    async fn run(&self, command: Command) -> Applied {
        self.0.commit(command).await.expect("a memory log commits")
    }

    async fn ask<R: Send>(&self, f: impl FnOnce(&MetaState) -> R + Send) -> R {
        self.0.query(f).await.expect("a memory log answers")
    }
}

impl MetaLog for Log {
    async fn commit(&self, command: Command) -> Result<Applied, MetaLogError> {
        self.0.commit(command).await
    }

    async fn query<R, F>(&self, f: F) -> Result<R, MetaLogError>
    where
        F: FnOnce(&MetaState) -> R + Send,
        R: Send,
    {
        self.0.query(f).await
    }
}

const PREFIX: &str = "kbf/log-a/";

/// A floor every file system the tests run on meets.
const FLOOR: u64 = 1;

fn prefix() -> KeyPrefix {
    KeyPrefix::new(PREFIX).expect("a prefix")
}

/// A fresh, absent directory for `name`, under the target directory.
fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-front-consistency")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn key(name: &str) -> ObjectKey {
    ObjectKey::new(name).expect("a key")
}

/// The key of object `seq` of epoch `epoch` under [`PREFIX`].
fn object_key(epoch: u64, seq: u64) -> ObjectKey {
    key(&format!("{PREFIX}cas/{epoch:016x}/{seq:016x}"))
}

fn object(epoch: u64, seq: u64) -> ObjectId {
    ObjectId::new(Epoch::new(epoch), seq)
}

fn blob(text: &str) -> VerifiedBlob {
    VerifiedBlob::hashed(Bytes::copy_from_slice(text.as_bytes()))
}

fn digest(text: &str) -> Digest {
    kbf_segments::sha256(text.as_bytes())
}

async fn plant(files: &FsStore, key: &ObjectKey, len: usize) {
    files
        .put_new(key, Bytes::from(vec![7; len]), None)
        .await
        .expect("plant a file");
}

async fn present(cache: &Cache<Log, FsStore>, texts: &[&str]) -> Vec<bool> {
    let digests: Vec<Digest> = texts.iter().map(|t| digest(t)).collect();
    cache.present(&digests).await.expect("present")
}

async fn holds(files: &FsStore, key: &ObjectKey) -> bool {
    files.may_hold(key).await.expect("may_hold")
}

/// A cache of a new "process" over `log` and `files`: a new epoch.
async fn open(log: &Log, files: &FsStore) -> Cache<Log, FsStore> {
    Cache::open(log.clone(), files.clone(), prefix())
        .await
        .expect("open a cache")
}

async fn pass(cache: &Cache<Log, FsStore>, files: &FsStore) -> PassReport {
    cache
        .consistency_pass(files, PASS_OPENS_PER_SEC, |_| Ok(()))
        .await
        .expect("the pass")
}

/// Writes the action-cache entry `action` with result `result` and no closure.
async fn put_action(log: &Log, action: Digest, result: Digest) {
    let record = ActionRecord {
        result,
        closure: Closure::new(),
    };
    let applied = log
        .run(Command::PutAction {
            role: Role::Daemon,
            action,
            record,
        })
        .await;
    assert_eq!(applied, Applied::Action(Ok(())));
}

/// Stages object `seq` of epoch 1 on a tier, holding the blob `text`.
async fn stage(log: &Log, seq: u64, text: &str) {
    let tier = TierId::new(7);
    let added = log
        .run(Command::AddTier {
            server: ServerId::new(1),
            tier,
        })
        .await;
    assert_eq!(added, Applied::Tier(Ok(())));
    let staged = log
        .run(Command::PutStaged {
            tier,
            objects: vec![StagedPut {
                object: object(1, seq),
                end: 100,
                seal: true,
                blobs: vec![(digest(text), 0)],
            }],
        })
        .await;
    assert!(matches!(staged, Applied::Staged(Ok(_))), "{staged:?}");
}

/// Catches a pass that keeps an orphan: a file of an earlier process's epoch that no
/// index entry names (an upload whose commit never happened), which nothing else ever
/// deletes (mutant: the delete branch removed). The indexed file beside it stays.
#[tokio::test]
async fn pass_deletes_old_epoch_orphan() {
    let dir = scratch("orphan");
    let files = FsStore::open(&dir, FLOOR).expect("open");
    let log = Log::new();
    let old = open(&log, &files).await;
    old.store_blobs(vec![blob("indexed")]).await.expect("store");
    drop(old);
    let orphan = object_key(1, 0xff);
    plant(&files, &orphan, 10).await;

    let cache = open(&log, &files).await;
    let report = pass(&cache, &files).await;

    assert!(!holds(&files, &orphan).await, "the orphan is still there");
    assert!(
        holds(&files, &object_key(1, 1)).await,
        "the indexed file went"
    );
    assert_eq!(present(&cache, &["indexed"]).await, [true]);
    assert_eq!((report.files, report.orphans_deleted), (2, 1));
    assert_eq!(report.kept, 1);
    assert_eq!(
        report.held.keys().copied().collect::<Vec<_>>(),
        [object(1, 1)]
    );
    drop((cache, files));
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// Catches a pass that deletes a file it must keep, though no index entry names it:
/// one of this process's epoch, written but not yet committed (mutant: the epoch check
/// dropped); one under another log's prefix, or under this prefix but not an object
/// name (mutant: the prefix check dropped, so another log's object reads as an orphan
/// of ours); a staged object whose blobs were collected, which the flusher has just
/// written (mutant: the staged check dropped); and a file with no header. Foreign and
/// headerless files are counted, never held.
#[tokio::test]
async fn pass_keeps_current_epoch_staged_foreign() {
    let dir = scratch("keeps");
    let files = FsStore::open(&dir, FLOOR).expect("open");
    let log = Log::new();
    let old = open(&log, &files).await;
    stage(&log, 0x50, "staged and then collected").await;
    log.run(Command::Tick(FarmTime::from_millis(u64::MAX / 2)))
        .await;
    assert!(matches!(
        log.run(Command::Collect).await,
        Applied::Collected(c) if c.blobs.len() == 1
    ));
    old.store_blobs(vec![blob("indexed")]).await.expect("store");
    drop(old);
    let staged = object_key(1, 0x50);
    plant(&files, &staged, 10).await;
    let foreign = [
        key("kbf/log-b/cas/0000000000000001/00000000000000ee"),
        key(&format!("{PREFIX}readyz-probe")),
        key(&format!("{PREFIX}cas/no-slash")),
        key(&format!("{PREFIX}cas/zz/0000000000000001")),
        key(&format!("{PREFIX}cas/0000000000000001/zz")),
        key(&format!("{PREFIX}cas/1/ee")),
    ];
    for k in &foreign {
        plant(&files, k, 10).await;
    }
    let headerless = dir.join("objects").join("00").join("headerless");
    std::fs::write(&headerless, b"").expect("plant a file with no header");

    let cache = open(&log, &files).await;
    assert_eq!(cache.epoch(), Epoch::new(2));
    let current = object_key(2, 0x77);
    plant(&files, &current, 10).await;
    let report = pass(&cache, &files).await;

    for k in [&staged, &current].into_iter().chain(&foreign) {
        assert!(holds(&files, k).await, "{k} was deleted");
    }
    assert!(headerless.exists(), "the headerless file was deleted");
    assert_eq!(report.orphans_deleted, 0);
    assert_eq!(report.foreign, foreign.len() as u64);
    assert_eq!(report.unreadable, 1);
    assert_eq!(report.files, 3 + foreign.len() as u64);
    assert_eq!(
        report.held.keys().copied().collect::<Vec<_>>(),
        [object(1, 1), object(1, 0x50), object(2, 0x77)]
    );
    assert_eq!(report.kept, 3);
    drop((cache, files));
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// [`Log`], and a flush that lands at its `at`th query: the flusher writes a staged
/// object's file and commits `ObjectFlushed` while the pass runs.
struct FlushAt {
    log: Log,
    at: usize,
    queries: AtomicUsize,
    flush: Mutex<Option<(FsStore, ObjectKey, ObjectId)>>,
}

impl MetaLog for FlushAt {
    async fn commit(&self, command: Command) -> Result<Applied, MetaLogError> {
        self.log.commit(command).await
    }

    async fn query<R, F>(&self, f: F) -> Result<R, MetaLogError>
    where
        F: FnOnce(&MetaState) -> R + Send,
        R: Send,
    {
        if self.queries.fetch_add(1, Ordering::Relaxed) + 1 == self.at {
            let flush = self
                .flush
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            let (files, key, object) = flush.expect("one flush");
            plant(&files, &key, 10).await;
            let flushed = self.log.run(Command::ObjectFlushed { object }).await;
            assert_eq!(flushed, Applied::Tier(Ok(())));
        }
        self.log.query(f).await
    }
}

/// Catches a pass that forgets an object because the walk did not see its file, when
/// the file is there by the time it decides (mutant: no second look before the
/// eviction): a staged object of an earlier process that the flusher writes to the
/// store and marks flushed after the walk passed its shard. Its blob must stay.
#[tokio::test]
async fn pass_keeps_an_object_flushed_after_its_shard_was_walked() {
    let dir = scratch("flushed-late");
    let files = FsStore::open(&dir, FLOOR).expect("open");
    let log = Log::new();
    let old = open(&log, &files).await;
    old.store_blobs(vec![blob("indexed")]).await.expect("store");
    drop(old);
    // An object whose file lands in shard 00, the first one walked.
    let seq = (0x100..)
        .find(|seq| kbf_segments::sha256(object_key(1, *seq).as_str().as_bytes()).hash[0] == 0)
        .expect("a key in shard 00");
    stage(&log, seq, "flushed late").await;
    let flush_at = FlushAt {
        log: log.clone(),
        // The index read, then shard 00's staged check: shard 00 has been walked.
        at: 2,
        queries: AtomicUsize::new(0),
        flush: Mutex::new(Some((files.clone(), object_key(1, seq), object(1, seq)))),
    };

    let cache = Cache::open(flush_at, files.clone(), prefix())
        .await
        .expect("open a cache");
    let report = cache
        .consistency_pass(&files, PASS_OPENS_PER_SEC, |_| Ok(()))
        .await
        .expect("the pass");

    let flushed = cache.meta().flush.lock().expect("flush").is_none();
    assert!(flushed, "the flush never ran");
    assert!(holds(&files, &object_key(1, seq)).await);
    let held = cache
        .present(&[digest("flushed late")])
        .await
        .expect("present");
    assert_eq!(held, [true]);
    assert_eq!(report.evicted_objects, 0);
    drop((cache, files));
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// Catches index entries whose object file is gone kept after the pass (mutant: the
/// index side skipped): the blob stays reported present, so a client never uploads
/// it again, and the action-cache entry naming it stays. Also a staged object with no
/// file in the store (its copy is a tier's block) sent for eviction (mutant: the staged
/// filter dropped), and a blob in another object, or an entry that needs none of the
/// lost blobs, taken with it.
#[tokio::test]
async fn pass_evicts_index_entry_without_file() {
    let dir = scratch("evicts");
    let files = FsStore::open(&dir, FLOOR).expect("open");
    let log = Log::new();
    let old = open(&log, &files).await;
    old.store_blobs(vec![blob("lost")]).await.expect("store");
    old.store_blobs(vec![blob("kept")]).await.expect("store");
    drop(old);
    stage(&log, 0x50, "staged").await;
    let (needs_lost, needs_kept) = (digest("action needing lost"), digest("action needing kept"));
    put_action(&log, needs_lost, digest("lost")).await;
    put_action(&log, needs_kept, digest("kept")).await;
    files.delete(&object_key(1, 1)).await.expect("delete");
    let losses = log.ask(MetaState::losses).await;

    let cache = open(&log, &files).await;
    let report = pass(&cache, &files).await;

    let held = present(&cache, &["lost", "kept", "staged"]).await;
    assert_eq!(held, [false, true, true]);
    let lost = log.ask(|s| s.action(&needs_lost)).await;
    assert_eq!(lost, ActionAnswer::Miss(Miss::NoEntry));
    let kept = log.ask(|s| s.action(&needs_kept)).await;
    assert!(matches!(kept, ActionAnswer::Hit { .. }), "{kept:?}");
    assert_eq!(log.ask(MetaState::losses).await, losses + 1);
    assert_eq!(
        (
            report.evicted_objects,
            report.evicted_blobs,
            report.evicted_actions,
            report.withheld
        ),
        (1, 1, 1, 0)
    );
    drop((cache, files));
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// Catches the pass wiping the index when the directory is not the one the index was
/// written over (an unmounted disk: `FsStore::open` creates the directory empty), by
/// forgetting every entry for want of its file (mutant: the guard removed). It must
/// drop nothing, and report how many objects it held back.
#[tokio::test]
async fn pass_drops_nothing_on_empty_dir() {
    let (dir, empty) = (scratch("guard-full"), scratch("guard-empty"));
    let full = FsStore::open(&dir, FLOOR).expect("open");
    let log = Log::new();
    let old = open(&log, &full).await;
    old.store_blobs(vec![blob("one")]).await.expect("store");
    old.store_blobs(vec![blob("two")]).await.expect("store");
    drop(old);
    put_action(&log, digest("action"), digest("one")).await;
    let before = log.ask(Clone::clone).await;
    let files = FsStore::open(&empty, FLOOR).expect("open");

    let cache = open(&log, &files).await;
    let report = pass(&cache, &files).await;

    assert_eq!(report.withheld, 2);
    assert_eq!(report.evicted_objects, 0);
    assert_eq!(present(&cache, &["one", "two"]).await, [true, true]);
    let after = log.ask(Clone::clone).await;
    assert_eq!(
        (after.blob_count(), after.action_count(), after.losses()),
        (before.blob_count(), before.action_count(), before.losses())
    );
    drop((cache, files, full));
    std::fs::remove_dir_all(&dir).expect("remove");
    std::fs::remove_dir_all(&empty).expect("remove");
}

/// Catches held bytes that count each file's header (mutant: the scan reports the
/// file's length, not the object's), so an evictor would free less than it thinks,
/// or that count a foreign file; and a modification time that is not the file's.
#[tokio::test]
async fn held_bytes_match_payloads() {
    let dir = scratch("held");
    let files = FsStore::open(&dir, FLOOR).expect("open");
    let log = Log::new();
    let cache = open(&log, &files).await;
    let before = SystemTime::now() - Duration::from_secs(2);
    plant(&files, &object_key(1, 0x10), 10).await;
    plant(&files, &object_key(1, 0x11), 300).await;
    plant(
        &files,
        &key("kbf/log-b/cas/0000000000000001/0000000000000001"),
        1000,
    )
    .await;

    let report = pass(&cache, &files).await;

    assert_eq!(report.held_bytes(), 310);
    let lens: Vec<u64> = report.held.values().map(|h| h.len).collect();
    assert_eq!(lens, [10, 300]);
    let after = SystemTime::now() + Duration::from_secs(2);
    for held in report.held.values() {
        assert!(before <= held.mtime && held.mtime <= after, "{held:?}");
    }
    drop((cache, files));
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// Catches a pass that ignores its rate (mutant: no wait), so a start of a large
/// directory takes the disk from the reads it serves; and one that keeps its pace while
/// the store is under its free-space floor (mutant: the floor check dropped), when
/// every upload is refused until it is done. Six files at six opens a second take a
/// second; the same six at one a second under the floor take no time.
#[tokio::test]
async fn pass_keeps_its_pace_unless_under_the_floor() {
    let dir = scratch("pace");
    let files = FsStore::open(&dir, FLOOR).expect("open");
    for seq in 1..=6 {
        plant(&files, &object_key(1, seq), 10).await;
    }
    let log = Log::new();
    let cache = open(&log, &files).await;
    let rate = |n| NonZeroU32::new(n).expect("not zero");

    let started = Instant::now();
    let paced = cache
        .consistency_pass(&files, rate(6), |_| Ok(()))
        .await
        .expect("the pass");
    assert!(started.elapsed() >= Duration::from_millis(900), "{paced:?}");
    assert_eq!(paced.kept, 6);
    drop((cache, files));

    let files = FsStore::open(&dir, u64::MAX).expect("open under the floor");
    assert!(files.under_floor().expect("statvfs"));
    let cache = open(&log, &files).await;
    let started = Instant::now();
    cache
        .consistency_pass(&files, rate(1), |_| Ok(()))
        .await
        .expect("the pass");
    assert!(started.elapsed() < Duration::from_secs(3));
    drop((cache, files));
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// Catches failures turned into a quiet success: a log that cannot be readied for
/// the eviction (a Raft log that cannot be raised to the format that carries it) fails
/// the pass and forgets nothing; a shard that cannot be listed fails it.
#[tokio::test]
async fn pass_failures_are_errors() {
    let dir = scratch("fails");
    let files = FsStore::open(&dir, FLOOR).expect("open");
    let log = Log::new();
    let old = open(&log, &files).await;
    old.store_blobs(vec![blob("lost")]).await.expect("store");
    old.store_blobs(vec![blob("kept")]).await.expect("store");
    drop(old);
    files.delete(&object_key(1, 1)).await.expect("delete");
    let cache = open(&log, &files).await;

    let refused = cache
        .consistency_pass(&files, PASS_OPENS_PER_SEC, |_| {
            Err(MetaLogError::Unavailable("not raised".into()))
        })
        .await;
    assert!(matches!(refused, Err(CacheError::Meta(_))), "{refused:?}");
    assert_eq!(present(&cache, &["lost"]).await, [true]);

    let empty_shard = std::fs::read_dir(dir.join("objects"))
        .expect("list the shards")
        .map(|e| e.expect("a shard").path())
        .find(|p| std::fs::remove_dir(p).is_ok());
    assert!(empty_shard.is_some(), "no empty shard to remove");
    let failed = cache
        .consistency_pass(&files, PASS_OPENS_PER_SEC, |_| Ok(()))
        .await;
    assert!(matches!(failed, Err(CacheError::Store(_))), "{failed:?}");
    drop((cache, files));
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// [`Log`], answering `EvictObjects` with another command's outcome: a broken log.
struct Misanswers(Log);

impl MetaLog for Misanswers {
    async fn commit(&self, command: Command) -> Result<Applied, MetaLogError> {
        match command {
            Command::EvictObjects { .. } => Ok(Applied::Collected(Collected::default())),
            other => self.0.commit(other).await,
        }
    }

    async fn query<R, F>(&self, f: F) -> Result<R, MetaLogError>
    where
        F: FnOnce(&MetaState) -> R + Send,
        R: Send,
    {
        self.0.query(f).await
    }
}

/// Catches a pass that counts an eviction the log answered with something else as
/// done: it must fail as an internal error.
#[tokio::test]
async fn pass_refuses_a_log_that_misanswers_the_eviction() {
    let dir = scratch("misanswer");
    let files = FsStore::open(&dir, FLOOR).expect("open");
    let log = Log::new();
    let old = open(&log, &files).await;
    old.store_blobs(vec![blob("lost")]).await.expect("store");
    old.store_blobs(vec![blob("kept")]).await.expect("store");
    drop(old);
    files.delete(&object_key(1, 1)).await.expect("delete");
    let cache = Cache::open(Misanswers(log), files.clone(), prefix())
        .await
        .expect("open a cache");

    let failed = cache
        .consistency_pass(&files, PASS_OPENS_PER_SEC, |_| Ok(()))
        .await;

    assert!(matches!(failed, Err(CacheError::Internal(_))), "{failed:?}");
    drop((cache, files));
    std::fs::remove_dir_all(&dir).expect("remove");
}
