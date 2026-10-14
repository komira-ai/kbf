//! The write tier's flusher task in the server ([`kbf_server::write_tier::run_flusher`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use kbf_front::{Cache, MemoryMetaLog, MetaLog, MetaLogError, VerifiedBlob};
use kbf_meta::{Applied, BlobAnswer, Command, MetaState, Retention, ServerId};
use kbf_objstore::{Capabilities, KeyPrefix, MemoryStore};
use kbf_server::write_tier::run_flusher;
use kbf_store::FaultFs;
use kbf_tier::TierOptions;

/// A memory log that answers UNAVAILABLE while `down`; clones share it.
#[derive(Clone, Debug)]
struct DownLog(Arc<(MemoryMetaLog, AtomicBool)>);

impl DownLog {
    fn new() -> Self {
        Self(Arc::new((
            MemoryMetaLog::new(Retention::default()),
            AtomicBool::new(false),
        )))
    }

    fn set_down(&self, down: bool) {
        self.0.1.store(down, Ordering::SeqCst);
    }

    fn inner(&self) -> Result<&MemoryMetaLog, MetaLogError> {
        if self.0.1.load(Ordering::SeqCst) {
            return Err(MetaLogError::Unavailable("down for the test".to_owned()));
        }
        Ok(&self.0.0)
    }
}

impl MetaLog for DownLog {
    async fn commit(&self, command: Command) -> Result<Applied, MetaLogError> {
        self.inner()?.commit(command).await
    }

    async fn query<R, F>(&self, f: F) -> Result<R, MetaLogError>
    where
        F: FnOnce(&MetaState) -> R + Send,
        R: Send,
    {
        self.inner()?.query(f).await
    }
}

/// Catches: a flusher task that stops at the first round that fails (the metadata log
/// down for a moment would leave every later block unflushed until a restart), and one
/// that never runs a round.
#[tokio::test(start_paused = true)]
async fn the_flusher_keeps_running_through_a_failed_round() {
    let log = DownLog::new();
    let store = MemoryStore::new(Capabilities::default());
    let cache = Cache::open(log.clone(), store, KeyPrefix::default())
        .await
        .expect("open")
        .with_segment_limit(1 << 10);
    let (cache, _) = cache
        .attach_tier(
            FaultFs::new(),
            TierOptions::new(1 << 20),
            ServerId::new(1),
            None,
        )
        .await
        .expect("attach");
    let cache = Arc::new(cache);
    let blob = VerifiedBlob::hashed(Bytes::from(vec![7; 4096]));
    let digest = blob.digest();
    cache.store_blobs(vec![blob]).await.expect("stored");
    assert!(
        staged(&log, digest).await,
        "a block of its own, sealed by its commit"
    );

    log.set_down(true);
    let flusher = tokio::spawn(run_flusher(Arc::clone(&cache), Duration::from_secs(1)));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    log.set_down(false);
    assert!(
        staged(&log, digest).await,
        "nothing flushed while the log was down"
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!staged(&log, digest).await, "a later round flushed it");
    assert!(!flusher.is_finished());
    flusher.abort();
}

/// Whether `digest` is staged on a tier, as `log` holds it.
async fn staged(log: &DownLog, digest: kbf_types::Digest) -> bool {
    let answer = log.query(move |s| s.blob(&digest)).await.expect("query");
    matches!(
        answer,
        BlobAnswer::Present {
            staged_on: Some(_),
            ..
        }
    )
}
