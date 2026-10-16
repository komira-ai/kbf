//! [`RoutedCas`]: the daemon's data plane over the storage map.
//!
//! The same [`Cas`] a runtime uses against [`crate::Transfers`], but each blob goes to
//! the storage nodes that own it (`kbf_route::Router`): reads from the owners in rank
//! order (callers of one blob at once share one read), writes to the primary, FindMissing
//! split by primary. A daemon holds a connection to a frontend for work, and to storage
//! nodes for bytes.
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
//!   does not apply. Reads into a file stream, and hold one chunk.

#![expect(dead_code, reason = "the red-first commit: nothing is routed yet")]

use std::future::Future;
use std::sync::Arc;

use kbf_proto::reapi::Digest;
use kbf_route::Router;
use kbf_storage::StorageError;
use kbf_types::{DigestFunction, LeaseId};
use tokio::sync::Semaphore;

use crate::cas::{Cas, CasError, label};
use crate::config::TransferConfig;

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

/// `digest` as storage names it, or `None` when it is not a SHA-256 digest with a size,
/// which no store holds.
fn parse(digest: &Digest) -> Option<kbf_types::Digest> {
    kbf_types::Digest::parse(DigestFunction::Sha256, &label(digest)).ok()
}

/// A storage failure as the data plane's: only a blob the farm does not hold is missing.
fn failed(blob: String, error: &StorageError) -> CasError {
    match error {
        StorageError::NotFound(_) => CasError::Missing(blob),
        other => CasError::Unavailable(blob, other.to_string()),
    }
}

impl Cas for RoutedCas {
    type Lease = Arc<Self>;

    fn lease(self: &Arc<Self>, _: &LeaseId) -> Arc<Self> {
        Arc::clone(self)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        Err(CasError::Unavailable(
            label(digest),
            "not routed yet".to_owned(),
        ))
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        Err(CasError::Unavailable(
            format!("of {} bytes", bytes.len()),
            "not routed yet".to_owned(),
        ))
    }

    async fn read_to(&self, digest: &Digest, _: &mut tokio::fs::File) -> Result<(), CasError> {
        Err(CasError::Unavailable(
            label(digest),
            "not routed yet".to_owned(),
        ))
    }

    fn in_flight(&self) -> usize {
        self.in_flight
    }
}
