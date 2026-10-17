//! [`RoutedCas`]: the daemon's data plane over the storage map.
//!
//! The same [`Cas`] a runtime uses against [`crate::Transfers`], but each blob goes to
//! the storage nodes that own it (`kbf_route::Router`): reads from the owners in rank
//! order (callers of one blob at once share one read), writes to the first owner that is
//! not down, FindMissing split by primary. A daemon holds a connection to a frontend for
//! work, and to storage nodes for bytes.
//!
//! What it keeps from the engine: at most `--transfer-in-flight` requests at once for the
//! process, FindMissing in calls of at most `--transfer-find-missing-chunk` digests, a
//! failed FindMissing answering "missing", and every blob checked against its digest.
//!
//! What it does not have yet:
//! - **Segment uploads.** [`Cas::put_records`] is the trait's default, a blob at a time,
//!   because a segment upload goes to one node and an action's outputs have different
//!   owners; one segment session per owner is the next step.
//! - **Streamed writes.** [`Cas::put_chunks`] is the trait's default, which gathers the
//!   chunks, because the storage client has no streamed write ([`kbf_storage::Storage`]
//!   takes a blob whole).
//! - **A byte budget** for reads into memory ([`Cas::get`]); `--transfer-bytes-in-flight`
//!   does not apply. Reads into a file stream, and hold one chunk; a copy that turns out
//!   not to be the blob is cut off the file and the next owner is read from the start.

use std::future::Future;
use std::io::SeekFrom;
use std::sync::Arc;

use bytes::Bytes;
use futures::future::join_all;
use kbf_proto::reapi::Digest;
use kbf_route::{BlobSink, IntoError, Router};
use kbf_storage::StorageError;
use kbf_types::{DigestFunction, LeaseId};
use tokio::io::{AsyncSeekExt as _, AsyncWriteExt as _};
use tokio::sync::Semaphore;

use crate::cas::{Cas, CasError, label};
use crate::config::TransferConfig;
use crate::spool::to_proto;

/// The daemon's data plane over the storage map. Cheap to clone through its `Arc`.
pub struct RoutedCas {
    router: Router,
    /// Requests in flight for the whole process.
    requests: Semaphore,
    in_flight: usize,
    find_missing_chunk: usize,
}

impl RoutedCas {
    /// A data plane calling the nodes through `router`, within the limits `config` names.
    #[must_use]
    pub fn new(router: Router, config: &TransferConfig) -> Self {
        let in_flight = config.in_flight.clamp(1, Semaphore::MAX_PERMITS);
        Self {
            router,
            requests: Semaphore::new(in_flight),
            in_flight,
            find_missing_chunk: config.find_missing_chunk.max(1),
        }
    }

    /// Runs `call` once a request permit is free. The semaphore is never closed.
    async fn limited<T>(&self, call: impl Future<Output = T>) -> T {
        let _permit = self.requests.acquire().await;
        call.await
    }
}

/// A file a blob is read into, from the offset it had when the read began.
struct FileSink<'a> {
    file: &'a mut tokio::fs::File,
    start: u64,
}

impl BlobSink for FileSink<'_> {
    type Error = std::io::Error;

    async fn write(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        self.file.write_all(chunk).await
    }

    /// Cuts the file back to where the read began and goes back there.
    async fn discard(&mut self) -> std::io::Result<()> {
        self.file.set_len(self.start).await?;
        self.file.seek(SeekFrom::Start(self.start)).await.map(drop)
    }
}

/// `digest` as storage names it, or `None` when it is not a SHA-256 digest with a size,
/// which no store holds.
fn parse(digest: &Digest) -> Option<kbf_types::Digest> {
    kbf_types::Digest::parse(DigestFunction::Sha256, &label(digest)).ok()
}

/// A storage failure as the data plane's: only a blob the farm does not hold is missing,
/// and only bytes that are not the blob are corrupt.
fn failed(blob: String, error: &StorageError) -> CasError {
    match error {
        StorageError::NotFound(_) => CasError::Missing(blob),
        StorageError::Corrupt { actual, .. } => CasError::Corrupt(blob, label(&to_proto(actual))),
        other => CasError::Unavailable(blob, other.to_string()),
    }
}

impl Cas for RoutedCas {
    type Lease = Arc<Self>;

    fn lease(self: &Arc<Self>, _: &LeaseId) -> Arc<Self> {
        Arc::clone(self)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        let blob = label(digest);
        let Some(parsed) = parse(digest) else {
            return Err(CasError::Missing(blob));
        };
        match self.limited(self.router.read_blob(&parsed)).await {
            Ok(bytes) => Ok(bytes.to_vec()),
            Err(e) => Err(failed(blob, &e)),
        }
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        let blob = format!("of {} bytes", bytes.len());
        match self.limited(self.router.put(Bytes::from(bytes))).await {
            Ok(digest) => Ok(to_proto(&digest)),
            Err(e) => Err(failed(blob, &e)),
        }
    }

    /// Streams the blob into `file` from its offset, one chunk at a time, through
    /// [`Router::read_blob_into`]: the owners are tried as [`Cas::get`] tries them, and
    /// an owner's copy that is not the blob is cut off the file before the next owner's is
    /// written. On every error the file holds what it held before, and its offset is
    /// where it was.
    async fn read_to(&self, digest: &Digest, file: &mut tokio::fs::File) -> Result<(), CasError> {
        let blob = label(digest);
        let Some(parsed) = parse(digest) else {
            return Err(CasError::Missing(blob));
        };
        let start = file.stream_position().await;
        let start = start.map_err(|e| CasError::Write(blob.clone(), e.to_string()))?;
        let _permit = self.requests.acquire().await;
        let mut sink = FileSink { file, start };
        let read = match self.router.read_blob_into(&parsed, &mut sink).await {
            Ok(()) => sink.file.flush().await,
            Err(IntoError::Storage(e)) => return Err(failed(blob, &e)),
            Err(IntoError::Sink(e)) => Err(e),
        };
        let Err(error) = read else {
            return Ok(());
        };
        // What was written may be in the file: it goes, if the file still lets it.
        let _ = sink.discard().await;
        Err(CasError::Write(blob, error.to_string()))
    }

    async fn missing(&self, digests: &[Digest]) -> Vec<Digest> {
        // A digest that does not parse is held by no store: ask nothing about it.
        let Some(parsed) = digests.iter().map(parse).collect::<Option<Vec<_>>>() else {
            return digests.to_vec();
        };
        let calls = parsed
            .chunks(self.find_missing_chunk)
            .map(|chunk| async move {
                let asked = self.limited(self.router.find_missing(chunk)).await;
                // A call that fails answers "missing" for its digests: FindMissing only
                // saves uploads.
                asked.unwrap_or_else(|_| chunk.to_vec())
            });
        join_all(calls)
            .await
            .into_iter()
            .flatten()
            .map(|d| to_proto(&d))
            .collect()
    }

    fn in_flight(&self) -> usize {
        self.in_flight
    }
}
