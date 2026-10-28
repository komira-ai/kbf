//! The start-up consistency pass over a `file://` cold store (`docs/deploy/storage-node.md`,
//! "The start-up pass"): `kbf-front`'s [`Cache::consistency_pass`] spawned once the
//! write tier is attached, never awaited before the server binds or reports ready;
//! then, in the same task, the evictor ([`Cache::evict_round`] every [`EVICT_EVERY`],
//! and at once when a write is refused for want of room), seeded with what the pass
//! kept.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use kbf_front::{Cache, EVICT_EVERY, MetaLog, PASS_OPENS_PER_SEC};
use kbf_objstore::{FsStore, ObjectStore};

/// What a failed pass logs.
const PASS_FAILED: &str = "start-up pass over the cold store directory failed; the next start runs it again, and \
     nothing is evicted until then";

/// What a failed round of eviction logs.
const ROUND_FAILED: &str = "eviction round failed; the next one tries again";

/// A pass to run over the file store `files` once the server is assembled, and the
/// evictor after it. Both commit `EvictObjects` to the log as it is, when there is
/// something to forget: nothing raises the log's machine version first, and the count of
/// its voters changes nothing.
pub(crate) struct StartPass {
    files: FsStore,
    /// How long after its last use an object is never evicted.
    grace: Duration,
    /// What the pass waits for before it starts: nothing in the server; a test holds
    /// it to see the server serve first.
    hold: Pin<Box<dyn Future<Output = ()> + Send>>,
}

impl StartPass {
    /// The pass and evictor over `files`, sparing objects used within `grace`.
    pub(crate) fn new(files: FsStore, grace: Duration) -> Self {
        Self {
            files,
            grace,
            hold: Box::pin(std::future::ready(())),
        }
    }

    /// The same pass, started only once `hold` completes: the rebuild of the index, in
    /// the server; in a test, a release the test sends.
    pub(crate) fn held(mut self, hold: impl Future<Output = ()> + Send + 'static) -> Self {
        self.hold = Box::pin(hold);
        self
    }
}

/// Turns on `cache`'s recency, then runs `pass` over it in a task of its own, logs
/// what it did, and evicts until the task is aborted. A pass that fails is logged and
/// leaves eviction off; the next start runs it again.
pub(crate) fn spawn<M: MetaLog, O: ObjectStore + 'static>(
    cache: Arc<Cache<M, O>>,
    pass: StartPass,
) -> tokio::task::JoinHandle<()> {
    cache.track_recency();
    tokio::spawn(async move {
        let (files, grace, hold) = (pass.files, pass.grace, pass.hold);
        hold.await;
        match cache.consistency_pass(&files, PASS_OPENS_PER_SEC).await {
            Ok(report) => {
                tracing::info!(
                    files = report.files,
                    kept = report.kept,
                    orphans_deleted = report.orphans_deleted,
                    foreign = report.foreign,
                    unreadable = report.unreadable,
                    evicted_objects = report.evicted_objects,
                    evicted_blobs = report.evicted_blobs,
                    evicted_actions = report.evicted_actions,
                    withheld = report.withheld,
                    held_bytes = report.held_bytes(),
                    duration_ms = report.duration.as_millis() as u64,
                    "start-up pass over the cold store directory done"
                );
                cache.seed_recency(&report.held);
                loop {
                    if let Err(error) = cache.evict_round(&files, grace).await {
                        tracing::error!(%error, "{ROUND_FAILED}");
                    }
                    let _woken = tokio::time::timeout(EVICT_EVERY, files.refused_full()).await;
                }
            }
            Err(error) => tracing::error!(%error, "{PASS_FAILED}"),
        }
    })
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use bytes::Bytes;
    use clap::Parser;
    use kbf_front::EVICT_GRACE;
    use kbf_log::MachineVersion;
    use kbf_meta::{Applied, Command, Epoch, Location, ObjectId, Retention, StoreId};
    use kbf_objstore::{Floor, KeyPrefix, ObjectKey, Space};
    use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
    use kbf_proto::reapi::{self, FindMissingBlobsRequest};
    use kbf_testkit::dir::TestDir;
    use kbf_types::Digest;
    use tonic::transport::Channel;

    use super::*;
    use crate::start::assemble;
    use crate::test_log::{capture, capture_until};
    use crate::{Args, RaftMetaLog};

    /// How long the test waits for the server to bind.
    const BOUND: Duration = Duration::from_secs(10);

    /// What every line the pass ends with says.
    const PASS_LINES: &str = "pass over the cold store directory";

    fn object_key(prefix: &KeyPrefix, seq: u64) -> ObjectKey {
        prefix
            .key(&format!("cas/{:016x}/{seq:016x}", 1))
            .expect("a key")
    }

    async fn missing(channel: &Channel, digests: &[Digest]) -> Vec<String> {
        let request = FindMissingBlobsRequest {
            blob_digests: digests
                .iter()
                .map(|d| reapi::Digest {
                    hash: d.hash_hex(),
                    size_bytes: d.size_bytes as i64,
                })
                .collect(),
            ..FindMissingBlobsRequest::default()
        };
        let answer = ContentAddressableStorageClient::new(channel.clone())
            .find_missing_blobs(request)
            .await
            .expect("FindMissingBlobs");
        let missing = answer.into_inner().missing_blob_digests.into_iter();
        missing.map(|d| d.hash).collect()
    }

    /// Asserts that a server does not wait for the start-up pass before it binds, so a
    /// large directory holds up no start: with the pass held, the server binds and
    /// answers FindMissing, which still reports a blob whose object file is gone.
    /// Released, the pass deletes the old epoch's orphan and forgets the lost blob over
    /// the Raft log, as the log is (it is never raised), and logs what it did.
    #[tokio::test(flavor = "current_thread")]
    async fn serves_before_pass_finishes() {
        let logs = capture_until(PASS_LINES);
        let root = crate::test_dir::scratch_dir("kbf-server-pass");
        let (blobs, raft) = (root.join("blobs"), root.join("raft"));
        let args = Args::parse_from([
            "kbf-server".to_owned(),
            format!("--cold-store=file://{}", blobs.display()),
            format!("--meta-dir={}", raft.display()),
            "--listen=127.0.0.1:0".to_owned(),
            "--worker-listen=127.0.0.1:0".to_owned(),
        ]);
        let files = FsStore::open(&blobs, Floor::Bytes(1)).expect("open the store");
        let log = RaftMetaLog::open_dir(&raft, Retention::default(), "kbf/")
            .await
            .expect("open the log");
        let prefix = log.store_prefix();
        // An earlier process's epoch: an orphan, a lost object and a kept one.
        assert_eq!(
            log.commit(Command::AllocEpoch).await,
            Ok(Applied::Epoch(Epoch::new(1)))
        );
        let (lost, kept) = (kbf_segments::sha256(b"lost"), kbf_segments::sha256(b"kept"));
        let at = |seq| Location {
            store: StoreId::CONFIGURED,
            object: ObjectId::new(Epoch::new(1), seq),
            offset: 0,
        };
        let put = log
            .commit(Command::PutBlobs(vec![(lost, at(1)), (kept, at(2))]))
            .await;
        assert!(matches!(put, Ok(Applied::Blobs(Ok(_)))), "{put:?}");
        let orphan = object_key(&prefix, 0xff);
        for key in [&orphan, &object_key(&prefix, 2)] {
            let body = Bytes::from_static(b"some bytes");
            files.put_new(key, body, None).await.expect("plant");
        }
        let (release, held) = tokio::sync::oneshot::channel::<()>();
        let pass = StartPass::new(files.clone(), EVICT_GRACE).held(async {
            let _ = held.await;
        });

        let id = log.log_id();
        let assembled = assemble(
            &args,
            None,
            log,
            files.clone(),
            prefix,
            id,
            || Ok(std::future::pending::<()>()),
            Some(pass),
        );
        let server = tokio::time::timeout(BOUND, assembled)
            .await
            .expect("bound while the pass is held")
            .expect("assembled");
        let channel = Channel::from_shared(format!("http://{}", server.reapi))
            .expect("a URI")
            .connect_lazy();
        // Serves until aborted; its outcome is not looked at.
        let serving = tokio::spawn(async move { drop(server.serve().await) });
        assert!(missing(&channel, &[lost, kept]).await.is_empty());
        assert!(files.may_hold(&orphan).await.expect("may_hold"));

        release.send(()).expect("the pass is waiting");
        logs.ended().await;
        assert_eq!(missing(&channel, &[lost, kept]).await, [lost.hash_hex()]);
        assert!(!files.may_hold(&orphan).await.expect("may_hold"));
        let text = logs.text();
        for field in [
            "orphans_deleted=1",
            "evicted_objects=1",
            "evicted_blobs=1",
            "kept=1",
        ] {
            assert!(text.contains(field), "{field}: {text}");
        }
        serving.abort();
        drop(files);
    }

    /// Asserts that the pass forgets the objects whose files are gone over a Raft log that
    /// was never raised, with no raise of its machine version first: the log stays at
    /// version 1, the two lost objects are forgotten (their blobs read as missing), the
    /// kept object stays, and the pass logs what it evicted. The log has one voter; no
    /// count of voters is an input to the pass, so a cell of more would be treated the
    /// same.
    #[tokio::test(flavor = "current_thread")]
    async fn the_pass_forgets_lost_objects_on_a_log_never_raised() {
        let logs = capture_until(PASS_LINES);
        let root = crate::test_dir::scratch_dir("kbf-server-pass-no-raise");
        let files = FsStore::open(&root.join("blobs"), Floor::Bytes(1)).expect("open the store");
        let meta = RaftMetaLog::open_dir(&root.join("raft"), Retention::default(), "kbf/")
            .await
            .expect("open the log");
        assert_eq!(
            meta.commit(Command::AllocEpoch).await,
            Ok(Applied::Epoch(Epoch::new(1)))
        );
        let prefix = meta.store_prefix();
        let at = |seq| Location {
            store: StoreId::CONFIGURED,
            object: ObjectId::new(Epoch::new(1), seq),
            offset: 0,
        };
        let [lost, gone, kept] = [b"lost", b"gone", b"kept"].map(|t| kbf_segments::sha256(t));
        let put = meta.commit(Command::PutBlobs(vec![
            (lost, at(1)),
            (gone, at(3)),
            (kept, at(2)),
        ]));
        assert!(matches!(put.await, Ok(Applied::Blobs(Ok(_)))));
        let body = Bytes::from_static(b"some bytes");
        let planted = files.put_new(&object_key(&prefix, 2), body, None).await;
        planted.expect("plant");
        let cache = Arc::new(
            Cache::open(meta, files.clone(), prefix)
                .await
                .expect("a cache"),
        );

        // The task goes on as the evictor, which has nothing to do here.
        let task = spawn(
            Arc::clone(&cache),
            StartPass::new(files.clone(), EVICT_GRACE),
        );
        logs.ended().await;
        task.abort();

        let text = logs.text();
        assert!(text.contains("evicted_objects=2"), "{text}");
        assert_eq!(cache.meta().machine_version(), MachineVersion::V1);
        let present = cache.present(&[lost, gone, kept]).await.expect("present");
        assert_eq!(present, [false, false, true]);
        let blobs = cache.meta().query(|s| s.blob_count()).await;
        assert_eq!(blobs, Ok(1));
        drop((cache, files));
    }

    /// Asserts that the pass forgets lost objects the same way over a Raft log an earlier
    /// build raised to machine version 3, which it leaves at version 3: the log it reads
    /// is the log it commits to.
    #[tokio::test(flavor = "current_thread")]
    async fn the_pass_forgets_lost_objects_on_a_log_an_earlier_build_raised() {
        let logs = capture_until(PASS_LINES);
        let root = crate::test_dir::scratch_dir("kbf-server-pass-earlier-raise");
        let files = FsStore::open(&root.join("blobs"), Floor::Bytes(1)).expect("open the store");
        let meta = RaftMetaLog::open_dir(&root.join("raft"), Retention::default(), "kbf/")
            .await
            .expect("open the log");
        meta.raise_machine_version(MachineVersion::V3)
            .expect("the earlier build's raise");
        assert_eq!(
            meta.commit(Command::AllocEpoch).await,
            Ok(Applied::Epoch(Epoch::new(1)))
        );
        let prefix = meta.store_prefix();
        let at = |seq| Location {
            store: StoreId::CONFIGURED,
            object: ObjectId::new(Epoch::new(1), seq),
            offset: 0,
        };
        let lost = kbf_segments::sha256(b"lost");
        let kept = kbf_segments::sha256(b"kept");
        let put = meta.commit(Command::PutBlobs(vec![(lost, at(1)), (kept, at(2))]));
        assert!(matches!(put.await, Ok(Applied::Blobs(Ok(_)))));
        let body = Bytes::from_static(b"some bytes");
        let stored = files.put_new(&object_key(&prefix, 2), body, None).await;
        stored.expect("write the object file");
        let cache = Arc::new(
            Cache::open(meta, files.clone(), prefix)
                .await
                .expect("a cache"),
        );

        let task = spawn(
            Arc::clone(&cache),
            StartPass::new(files.clone(), EVICT_GRACE),
        );
        logs.ended().await;
        task.abort();

        let text = logs.text();
        assert!(text.contains("evicted_objects=1"), "{text}");
        assert_eq!(cache.meta().machine_version(), MachineVersion::V3);
        let present = cache.present(&[lost, kept]).await.expect("present");
        assert_eq!(present, [false, true]);
        drop((cache, files));
    }

    /// Asserts that a pass that fails is logged and ends: a shard of the store that cannot
    /// be listed (its directory is gone) fails the pass, which logs that it failed, with
    /// the cause, and its task ends without a panic.
    #[tokio::test(flavor = "current_thread")]
    async fn a_failed_pass_is_logged() {
        let logs = capture();
        let root = crate::test_dir::scratch_dir("kbf-server-pass-fails");
        let files = FsStore::open(&root.join("blobs"), Floor::Bytes(1)).expect("open the store");
        // A Raft log, as in the server: one instantiation of the task covers both arms.
        let meta = RaftMetaLog::open_dir(&root.join("raft"), Retention::default(), "kbf/")
            .await
            .expect("open the log");
        let prefix = meta.store_prefix();
        let cache = Cache::open(meta, files.clone(), prefix)
            .await
            .expect("a cache");
        let shard = std::fs::read_dir(root.join("blobs").join("objects"))
            .expect("list the shards")
            .map(|e| e.expect("a shard").path())
            .find(|p| std::fs::remove_dir(p).is_ok());
        assert!(shard.is_some(), "no empty shard to remove");

        spawn(Arc::new(cache), StartPass::new(files, EVICT_GRACE))
            .await
            .expect("the pass task ends without a panic");

        let text = logs.text();
        assert!(text.contains(PASS_FAILED), "{text}");
        assert!(text.contains("No such file or directory"), "{text}");
    }

    /// A cache over a Raft log whose earlier epoch indexed the blob `kept` in object
    /// 2, whose file is in a store that sees a file system with nothing free (its
    /// floor 1000 bytes): what the pass keeps, and the evictor must then evict. While
    /// `broken` is set the file system cannot be asked how much room it has.
    async fn short_of_room(
        name: &str,
        broken: Arc<AtomicBool>,
    ) -> (TestDir, FsStore, Cache<RaftMetaLog, FsStore>) {
        let root = crate::test_dir::scratch_dir(&format!("kbf-server-{name}"));
        // Room to plant the object file, then none.
        let room = Arc::new(AtomicBool::new(true));
        let fake = Arc::clone(&room);
        let none_free = Arc::new(move |_: &std::path::Path| -> io::Result<Space> {
            if broken.load(Ordering::Relaxed) {
                return Err(io::Error::other("the file system cannot be asked"));
            }
            let available = if fake.load(Ordering::Relaxed) {
                1 << 20
            } else {
                0
            };
            Ok(Space {
                total: 1 << 30,
                available,
            })
        });
        let files = FsStore::open_with_space(&root.join("blobs"), Floor::Bytes(1000), none_free);
        let files = files.expect("open the store");
        let meta = RaftMetaLog::open_dir(&root.join("raft"), Retention::default(), "kbf/").await;
        let meta = meta.expect("open the log");
        assert_eq!(
            meta.commit(Command::AllocEpoch).await,
            Ok(Applied::Epoch(Epoch::new(1)))
        );
        let prefix = meta.store_prefix();
        let at = Location {
            store: StoreId::CONFIGURED,
            object: ObjectId::new(Epoch::new(1), 2),
            offset: 0,
        };
        let put = meta.commit(Command::PutBlobs(vec![(kbf_segments::sha256(b"kept"), at)]));
        assert!(matches!(put.await, Ok(Applied::Blobs(Ok(_)))));
        let body = Bytes::from_static(b"some bytes");
        files
            .put_new(&object_key(&prefix, 2), body, None)
            .await
            .expect("plant");
        room.store(false, Ordering::Relaxed);
        let cache = Cache::open(meta, files.clone(), prefix)
            .await
            .expect("a cache");
        (root, files, cache)
    }

    /// Asserts that the evictor is armed after the pass and seeded with what the pass
    /// kept (an object of unknown length is never evicted): with no room on the file
    /// system, the object the pass kept is forgotten, its file deleted, and the round
    /// logged. The Raft log is never raised for it: it is at machine version 1 when the
    /// round is done.
    #[tokio::test(flavor = "current_thread")]
    async fn the_evictor_runs_after_the_pass() {
        let logs = capture_until("evicted the least recently used objects");
        let (_root, files, cache) = short_of_room("evictor", Arc::default()).await;
        let key = cache
            .object_key(ObjectId::new(Epoch::new(1), 2))
            .expect("a key");
        let cache = Arc::new(cache);

        let task = spawn(
            Arc::clone(&cache),
            StartPass::new(files.clone(), EVICT_GRACE),
        );
        logs.ended().await;
        task.abort();

        let kept = kbf_segments::sha256(b"kept");
        assert_eq!(cache.present(&[kept]).await.expect("present"), [false]);
        assert!(!files.may_hold(&key).await.expect("may_hold"));
        assert_eq!(cache.meta().machine_version(), MachineVersion::V1);
        let text = logs.text();
        assert!(
            text.contains("objects=1 bytes=10 blobs=1 actions=0"),
            "{text}"
        );
        drop(files);
    }

    /// Catches a start-up pass that runs before the rebuild of the index is done (defect:
    /// the pass not held for the rebuild): it would walk the directory against an index
    /// that does not name the old server's objects yet, keep none of them, and seed the
    /// evictor with none, so the objects the rebuild then adopts would never be evicted.
    /// A node over a new log, registered as member 7, rebuilds over three segments of an
    /// old server in the directory: the rebuild is logged done before the pass is, and the
    /// pass keeps all three (`kept=3`, no foreign file).
    #[tokio::test(flavor = "current_thread")]
    async fn the_start_pass_waits_for_the_rebuild() {
        use kbf_front::{MemoryMetaLog, ObjectKeys, RebuildConfig, VerifiedBlob};
        use kbf_meta::NodeWriter;
        use kbf_storage::service::CatchUp;

        let logs = capture_until(PASS_LINES);
        let root = crate::test_dir::scratch_dir("kbf-server-pass-rebuild");
        let files = FsStore::open(&root.join("blobs"), Floor::Bytes(1)).expect("open the store");
        let old_keys = ObjectKeys {
            legacy: KeyPrefix::new(format!("kbf/{:016x}/", 0x11)).expect("prefix"),
            root: KeyPrefix::new("kbf/").expect("root"),
        };
        let old = Cache::open_under(
            MemoryMetaLog::new(Retention::default()),
            files.clone(),
            old_keys,
        )
        .await
        .expect("an old server's cache");
        for i in 0..3 {
            let blob = VerifiedBlob::hashed(Bytes::from(format!("old blob {i}")));
            old.store_blobs(vec![blob]).await.expect("store");
        }

        let meta = RaftMetaLog::open_dir(&root.join("raft"), Retention::default(), "kbf/")
            .await
            .expect("open the log");
        let keys = ObjectKeys {
            legacy: meta.store_prefix(),
            root: KeyPrefix::new("kbf/").expect("root"),
        };
        let one = std::num::NonZeroU64::MIN;
        let writer = NodeWriter::new(one, one);
        let cache = Cache::open_node(meta, files.clone(), keys, writer)
            .await
            .expect("a node's cache");
        let cache = Arc::new(cache);
        let catch_up = CatchUp::rebuilding();
        let pass = StartPass::new(files.clone(), EVICT_GRACE);
        let _background =
            crate::rebuild::spawn(&cache, &catch_up, Some(pass), RebuildConfig::default());
        logs.ended().await;

        assert!(!catch_up.is_catching_up());
        let text = logs.text();
        let rebuilt = text
            .find("rebuild of the index from the store done")
            .expect("the rebuild is logged");
        let passed = text
            .find("start-up pass over the cold store directory done")
            .expect("the pass is logged");
        assert!(rebuilt < passed, "{text}");
        for field in [
            "kept=3",
            "foreign=0",
            "orphans_deleted=0",
            "evicted_objects=0",
        ] {
            assert!(text.contains(field), "{field}: {text}");
        }
        drop((cache, old, files));
    }

    /// Asserts that a round of eviction that fails is logged: a file system that cannot
    /// be asked how much room it has fails the round, which is logged with the cause;
    /// nothing is forgotten and the file stays.
    #[tokio::test(flavor = "current_thread")]
    async fn a_failed_eviction_round_is_logged() {
        let logs = capture_until(ROUND_FAILED);
        let broken = Arc::new(AtomicBool::new(false));
        let (_root, files, cache) = short_of_room("evict-fails", Arc::clone(&broken)).await;
        let key = cache
            .object_key(ObjectId::new(Epoch::new(1), 2))
            .expect("a key");
        broken.store(true, Ordering::Relaxed);

        let task = spawn(Arc::new(cache), StartPass::new(files.clone(), EVICT_GRACE));
        logs.ended().await;
        task.abort();

        let cause = "the file system cannot be asked";
        assert!(logs.text().contains(cause), "{}", logs.text());
        assert!(files.may_hold(&key).await.expect("may_hold"));
        drop(files);
    }
}
