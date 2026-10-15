//! The start-up consistency pass under `--blob-dir` (`docs/deploy/storage-node.md`,
//! "The start-up pass"): `kbf-front`'s [`Cache::consistency_pass`] spawned once the
//! write tier is attached, never awaited before the server binds or reports ready.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use kbf_front::{Cache, EvictReady, MetaLog, MetaLogError, PASS_OPENS_PER_SEC};
use kbf_log::MachineVersion;
use kbf_objstore::{FsStore, ObjectStore};

use crate::RaftMetaLog;

/// What a failed pass logs.
const PASS_FAILED: &str =
    "start-up pass over the blob directory failed; the next start runs it again";

/// A pass to run over the file store `files` once the server is assembled.
pub(crate) struct StartPass<M> {
    files: FsStore,
    /// Readies the log for `EvictObjects`, if it may, called only when there is
    /// something to forget.
    ready_to_evict: fn(&M) -> Result<EvictReady, MetaLogError>,
    /// What the pass waits for before it starts: nothing in the server; a test holds
    /// it to see the server serve first.
    hold: Pin<Box<dyn Future<Output = ()> + Send>>,
}

/// Readies a Raft log for `EvictObjects` ([`ready_among`] its own voters).
fn ready_to_evict(log: &RaftMetaLog) -> Result<EvictReady, MetaLogError> {
    ready_among(log, log.voters())
}

/// Whether a Raft log in a cell of `voters` voters takes `EvictObjects`: one at
/// machine version 3 (whose format carries the command) does. One below it is raised
/// to it here only if `voters` is one (a single-host farm): a raise in a cell of more
/// would make every replica that does not run version 3 yet refuse the log, so there
/// the operator raises it once every replica does, and until then the answer is
/// [`EvictReady::NeedsV3`]. Called only when something is to be forgotten, so a log
/// with nothing to forget stays one an older binary reads.
fn ready_among(log: &RaftMetaLog, voters: usize) -> Result<EvictReady, MetaLogError> {
    if log.machine_version() >= MachineVersion::V3 {
        return Ok(EvictReady::Ready);
    }
    if voters > 1 {
        return Ok(EvictReady::NeedsV3 { voters });
    }
    log.raise_machine_version(MachineVersion::V3)?;
    Ok(EvictReady::Ready)
}

impl StartPass<RaftMetaLog> {
    /// The pass over `files` under a Raft log ([`ready_to_evict`]).
    pub(crate) fn over_raft(files: FsStore) -> Self {
        Self {
            files,
            ready_to_evict,
            hold: Box::pin(std::future::ready(())),
        }
    }
}

impl<M> StartPass<M> {
    /// The same pass, started only once `hold` completes.
    #[cfg(test)]
    pub(crate) fn held(mut self, hold: impl Future<Output = ()> + Send + 'static) -> Self {
        self.hold = Box::pin(hold);
        self
    }
}

/// Runs `pass` over `cache` in a task of its own, and logs what it did. A pass that
/// fails is logged; the next start runs it again.
pub(crate) fn spawn<M: MetaLog, O: ObjectStore + 'static>(
    cache: Arc<Cache<M, O>>,
    pass: StartPass<M>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let StartPass {
            files,
            ready_to_evict,
            hold,
        } = pass;
        hold.await;
        match cache
            .consistency_pass(&files, PASS_OPENS_PER_SEC, ready_to_evict)
            .await
        {
            Ok(report) => tracing::info!(
                files = report.files,
                kept = report.kept,
                orphans_deleted = report.orphans_deleted,
                foreign = report.foreign,
                unreadable = report.unreadable,
                evicted_objects = report.evicted_objects,
                evicted_blobs = report.evicted_blobs,
                evicted_actions = report.evicted_actions,
                marked_missing = report.marked_missing,
                withheld = report.withheld,
                held_bytes = report.held_bytes(),
                duration_ms = report.duration.as_millis() as u64,
                "start-up pass over the blob directory done"
            ),
            Err(error) => tracing::error!(%error, "{PASS_FAILED}"),
        }
    })
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Mutex, PoisonError};
    use std::time::Duration;

    use bytes::Bytes;
    use clap::Parser;
    use kbf_front::NEEDS_V3;
    use kbf_meta::{Applied, Command, Epoch, Location, ObjectId, Retention, StoreId};
    use kbf_objstore::{KeyPrefix, ObjectKey};
    use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
    use kbf_proto::reapi::{self, FindMissingBlobsRequest};
    use kbf_types::Digest;
    use tonic::transport::Channel;

    use super::*;
    use crate::Args;
    use crate::start::assemble;

    /// How long the test waits for the server to bind, or for the pass to finish.
    const BOUND: Duration = Duration::from_secs(10);

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

    /// Catches a server that waits for the start-up pass before it binds (mutant: the
    /// pass awaited in the assembly), so a large directory holds up every start: with
    /// the pass held, the server must bind and answer FindMissing, which still reports
    /// a blob whose object file is gone. Released, the pass must delete the old
    /// epoch's orphan and forget the lost blob over the Raft log, which proves the log
    /// is raised to the format that carries `EvictObjects` (mutant: no raise), and log
    /// what it did.
    #[tokio::test(flavor = "current_thread")]
    async fn serves_before_pass_finishes() {
        let logs = Capture::default();
        let _default = logs.subscribe();
        let root = std::env::temp_dir().join(format!("kbf-server-pass-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (blobs, raft) = (root.join("blobs"), root.join("raft"));
        let args = Args::parse_from([
            "kbf-server".as_ref(),
            "--blob-dir".as_ref(),
            blobs.as_os_str(),
            "--meta-log".as_ref(),
            "raft".as_ref(),
            "--raft-dir".as_ref(),
            raft.as_os_str(),
            "--listen".as_ref(),
            "127.0.0.1:0".as_ref(),
            "--worker-listen".as_ref(),
            "127.0.0.1:0".as_ref(),
        ]);
        let files = FsStore::open(&blobs, 1).expect("open the store");
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
        let pass = StartPass::over_raft(files.clone()).held(async {
            let _ = held.await;
        });

        let assembled = assemble(
            &args,
            log,
            files.clone(),
            prefix,
            None,
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
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Catches a pass that raises the log's machine version in a cell of more than
    /// one voter (mutant: always raise), which would make every replica that does not
    /// run version 3 yet refuse the log; one that forgets there anyway, or marks
    /// nothing missing; and one that logs the refusal per object (mutant: in the
    /// loop) rather than once. Three voters: the log stays at version 1, the two lost
    /// objects' blobs read as missing and one line says so. Once the operator raises
    /// the log, the next pass forgets them.
    #[tokio::test(flavor = "current_thread")]
    async fn a_cell_of_three_marks_missing_until_the_operator_raises() {
        let logs = Capture::default();
        let _default = logs.subscribe();
        let root =
            std::env::temp_dir().join(format!("kbf-server-pass-three-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let files = FsStore::open(&root.join("blobs"), 1).expect("open the store");
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
        let three = || StartPass {
            files: files.clone(),
            ready_to_evict: |log| ready_among(log, 3),
            hold: Box::pin(std::future::ready(())),
        };

        spawn(Arc::clone(&cache), three()).await.expect("the pass");

        let text = logs.text();
        assert_eq!(text.matches(NEEDS_V3).count(), 1, "{text}");
        assert!(
            text.contains(&format!("{NEEDS_V3}: 2 objects held")),
            "{text}"
        );
        assert!(text.contains("marked_missing=2"), "{text}");
        assert!(text.contains("evicted_objects=0"), "{text}");
        assert_eq!(cache.meta().machine_version(), MachineVersion::V1);
        let present = cache.present(&[lost, gone, kept]).await.expect("present");
        assert_eq!(present, [false, false, true]);
        let blobs = cache.meta().query(|s| s.blob_count()).await;
        assert_eq!(blobs, Ok(3), "forgotten below version 3");

        cache
            .meta()
            .raise_machine_version(MachineVersion::V3)
            .expect("the operator's raise");
        logs.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        spawn(Arc::clone(&cache), three()).await.expect("the pass");

        let text = logs.text();
        assert!(!text.contains(NEEDS_V3), "{text}");
        assert!(text.contains("evicted_objects=2"), "{text}");
        let blobs = cache.meta().query(|s| s.blob_count()).await;
        assert_eq!(blobs, Ok(1));
        drop((cache, files));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Catches a readiness that counts the log at version 3 as not ready in a cell of
    /// many (mutant: the version check dropped), or the boundary moved (mutants: `>= 1`
    /// or `> 2` for `> 1`): two voters need the operator, one is raised here.
    #[tokio::test(flavor = "current_thread")]
    async fn only_a_single_voter_is_raised_here() {
        let root =
            std::env::temp_dir().join(format!("kbf-server-pass-voters-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let log = RaftMetaLog::open_dir(&root, Retention::default(), "kbf/")
            .await
            .expect("open the log");
        let two = ready_among(&log, 2);
        assert_eq!(two, Ok(EvictReady::NeedsV3 { voters: 2 }));
        assert_eq!(log.machine_version(), MachineVersion::V1);
        assert_eq!(ready_to_evict(&log), Ok(EvictReady::Ready));
        assert_eq!(log.machine_version(), MachineVersion::V3);
        assert_eq!(ready_among(&log, 2), Ok(EvictReady::Ready));
        drop(log);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Log lines written by the test's subscriber, and a wake-up when the pass's
    /// last line is written.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>, Arc<tokio::sync::Notify>);

    impl Capture {
        /// Sends this thread's log lines here until the guard drops.
        fn subscribe(&self) -> tracing::subscriber::DefaultGuard {
            let writer = self.clone();
            let subscriber = tracing_subscriber::fmt()
                .with_writer(move || writer.clone())
                .with_ansi(false)
                .finish();
            tracing::subscriber::set_default(subscriber)
        }

        /// Waits, at most `BOUND`, for the pass's last line.
        async fn ended(&self) {
            let ended = tokio::time::timeout(BOUND, self.1.notified()).await;
            ended.expect("the pass logged no last line");
        }

        fn text(&self) -> String {
            let bytes = self
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            String::from_utf8(bytes).expect("UTF-8 logs")
        }
    }

    impl io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            held.extend_from_slice(bytes);
            let last = b"pass over the blob directory";
            if bytes.windows(last.len()).any(|w| w == last) {
                self.1.notify_one();
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Catches a pass whose failure is dropped without a word (mutant: the error arm
    /// logs nothing) or brings its task down: a log that cannot be readied for the
    /// eviction fails the pass, which must log that it failed, with the cause, and end.
    #[tokio::test(flavor = "current_thread")]
    async fn a_failed_pass_is_logged() {
        let mut logs = Capture::default();
        let _default = logs.subscribe();
        let root =
            std::env::temp_dir().join(format!("kbf-server-pass-fails-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let files = FsStore::open(&root.join("blobs"), 1).expect("open the store");
        // A Raft log, as in the server: one instantiation of the task covers both arms.
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
        let (lost, kept) = (kbf_segments::sha256(b"lost"), kbf_segments::sha256(b"kept"));
        let put = meta.commit(Command::PutBlobs(vec![(lost, at(1)), (kept, at(2))]));
        assert!(matches!(put.await, Ok(Applied::Blobs(Ok(_)))));
        let body = Bytes::from_static(b"some bytes");
        let planted = files.put_new(&object_key(&prefix, 2), body, None).await;
        planted.expect("plant");
        let cache = Cache::open(meta, files.clone(), prefix)
            .await
            .expect("a cache");
        let pass = StartPass {
            files,
            ready_to_evict: |_| Err(MetaLogError::Unavailable("not raised".to_owned())),
            hold: Box::pin(std::future::ready(())),
        };

        spawn(Arc::new(cache), pass)
            .await
            .expect("the pass task ends without a panic");

        io::Write::flush(&mut logs).expect("flush");
        let text = logs.text();
        assert!(text.contains(PASS_FAILED), "{text}");
        assert!(text.contains("not raised"), "{text}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
