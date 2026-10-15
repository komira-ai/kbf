//! The start-up consistency pass under `--blob-dir` (`docs/deploy/storage-node.md`,
//! "The start-up pass"): `kbf-front`'s [`Cache::consistency_pass`] spawned once the
//! write tier is attached, never awaited before the server binds or reports ready.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use kbf_front::{Cache, MetaLog, MetaLogError, PASS_OPENS_PER_SEC};
use kbf_log::MachineVersion;
use kbf_objstore::{FsStore, ObjectStore};

use crate::RaftMetaLog;

/// A pass to run over the file store `files` once the server is assembled.
pub(crate) struct StartPass<M> {
    files: FsStore,
    /// Readies the log for `EvictObjects`, called only when there is something to
    /// forget.
    ready_to_evict: fn(&M) -> Result<(), MetaLogError>,
    /// What the pass waits for before it starts: nothing in the server; a test holds
    /// it to see the server serve first.
    hold: Pin<Box<dyn Future<Output = ()> + Send>>,
}

impl StartPass<RaftMetaLog> {
    /// The pass over `files` under a Raft log, which is raised to machine version 3
    /// (whose format carries `EvictObjects`) the first time the pass forgets an
    /// object; a log with nothing to forget stays one an older binary reads.
    pub(crate) fn over_raft(files: FsStore) -> Self {
        Self {
            files,
            ready_to_evict: |log| log.raise_machine_version(MachineVersion::V3),
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
                withheld = report.withheld,
                held_bytes = report.held_bytes(),
                duration_ms = report.duration.as_millis() as u64,
                "start-up pass over the blob directory done"
            ),
            Err(error) => tracing::error!(
                %error,
                "start-up pass over the blob directory failed; the next start runs it again"
            ),
        }
    })
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use bytes::Bytes;
    use clap::Parser;
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
    /// is raised to the format that carries `EvictObjects` (mutant: no raise).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn serves_before_pass_finishes() {
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
        let serving = tokio::spawn(async move { server.serve().await.map_err(|e| e.to_string()) });
        assert!(missing(&channel, &[lost, kept]).await.is_empty());
        assert!(files.may_hold(&orphan).await.expect("may_hold"));

        release.send(()).expect("the pass is waiting");
        let deadline = Instant::now() + BOUND;
        while missing(&channel, &[lost, kept]).await != [lost.hash_hex()] {
            assert!(
                Instant::now() < deadline,
                "the pass did not forget the lost blob"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!files.may_hold(&orphan).await.expect("may_hold"));
        serving.abort();
        drop(files);
        let _ = std::fs::remove_dir_all(&root);
    }
}
