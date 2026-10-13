//! What the engine counts: [`TransferStats`] per lease (`docs/design/worker-cache.md#46-metrics-through-the-api`)
//! and [`EngineStats`] for the process.
//!
//! A lease's counters are logged once, when the last handle on them goes (its
//! [`crate::Scope`], and any fetch it started that is still finishing). Carrying them
//! to the API (`auxiliary_metadata`, the invocation metrics) is a later stage.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use kbf_types::LeaseId;

/// One lease's transfers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransferStats {
    /// ByteStream reads this lease started, each attempt counted (a retry is a read).
    pub read_calls: u64,
    /// Blob bytes those reads received.
    pub read_bytes: u64,
    /// Reads served by a fetch another caller (this lease or another) had started.
    pub merged_reads: u64,
    /// Digests this lease asked FindMissing about.
    pub find_missing_digests: u64,
    /// Uploads skipped because FindMissing said the server holds the blob.
    pub outputs_present: u64,
    /// ByteStream writes, each attempt counted.
    pub write_calls: u64,
    /// Blob bytes those writes sent.
    pub written_bytes: u64,
    /// Requests retried after an UNAVAILABLE or a broken connection.
    pub retries: u64,
}

impl fmt::Display for TransferStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "read_calls={} read_bytes={} merged_reads={} find_missing_digests={} \
             outputs_present={} write_calls={} written_bytes={} retries={}",
            self.read_calls,
            self.read_bytes,
            self.merged_reads,
            self.find_missing_digests,
            self.outputs_present,
            self.write_calls,
            self.written_bytes,
            self.retries
        )
    }
}

/// [`TransferStats`] as they are counted, and the lease they are logged for.
#[derive(Debug, Default)]
pub(crate) struct Counts {
    pub lease: Option<LeaseId>,
    pub read_calls: AtomicU64,
    pub read_bytes: AtomicU64,
    pub merged_reads: AtomicU64,
    pub find_missing_digests: AtomicU64,
    pub outputs_present: AtomicU64,
    pub write_calls: AtomicU64,
    pub written_bytes: AtomicU64,
    pub retries: AtomicU64,
}

impl Counts {
    pub fn new(lease: Option<LeaseId>) -> Self {
        let mut counts = Self::default();
        counts.lease = lease;
        counts
    }

    pub fn stats(&self) -> TransferStats {
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        TransferStats {
            read_calls: get(&self.read_calls),
            read_bytes: get(&self.read_bytes),
            merged_reads: get(&self.merged_reads),
            find_missing_digests: get(&self.find_missing_digests),
            outputs_present: get(&self.outputs_present),
            write_calls: get(&self.write_calls),
            written_bytes: get(&self.written_bytes),
            retries: get(&self.retries),
        }
    }
}

impl Drop for Counts {
    fn drop(&mut self) {
        if let Some(lease) = &self.lease {
            let stats = self.stats();
            tracing::info!(lease = %lease, "transfers: {stats}");
        }
    }
}

/// Adds `n` to `counter`.
pub(crate) fn add(counter: &AtomicU64, n: u64) {
    counter.fetch_add(n, Ordering::Relaxed);
}

/// The process's transfers, now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EngineStats {
    /// Requests holding a permit now.
    pub requests_in_flight: usize,
    /// Bytes of in-memory reads holding the byte budget now.
    pub bytes_in_flight: usize,
    /// Distinct blobs being fetched into memory now (each shared by its callers).
    pub fetches: usize,
    /// FindMissing calls made.
    pub find_missing_calls: u64,
    /// Times the server answered FindMissing with UNIMPLEMENTED; after the first, the
    /// engine stops asking and uploads every blob.
    pub find_missing_unavailable: u64,
}
