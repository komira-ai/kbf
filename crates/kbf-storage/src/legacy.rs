//! `kbf_storage_legacy_read_total`: the reads a storage node serves from its own copy of the
//! farm's data. The farm is moving blobs and action results to packs in an object store, and
//! this counter is how an operator sees that nothing reads the storage nodes any more before
//! the nodes are turned off: it stays at 0 for as long as no caller is served by a node.
//!
//! **What a legacy read is.** One call that a storage node ([`crate::service::StorageService`])
//! answered with data from its own store:
//!
//! - `kind="cas"`, `source="bytestream"`: a ByteStream `Read` of a blob the node holds, one per
//!   call (a client that reads on from an offset after a broken stream makes another call and
//!   counts again), counted when the node starts the answer, that is when the blob was found and
//!   its first window verified.
//! - `kind="cas"`, `source="segments"`: a `Segments` `ReadRange` the node answered with bytes,
//!   one per call (a call reads a run of blobs from one segment, so a call is not a blob).
//! - `kind="ac"`, `source="action_store"`: an `ActionStore` `Get` that found an entry.
//! - `kind="cas"` and `kind="ac"`, `source="in_process"`: the same three reads made by the
//!   daemon half of a `storage-and-daemon` process, which reaches its own node without a
//!   connection ([`crate::service::InProcess`]); a `Segments` read made that way is counted
//!   as `source="segments"`.
//!
//! **What is not counted.** A call that found nothing (a miss, an error, an offset past the end);
//! a read of the empty blob, which no store holds; `FindMissingBlobs` and `QueryWriteStatus`
//! (they ask whether a blob is held and return none of it); `Segments` `Locate`; every write;
//! and the node's own reads of its disk for flushing, repair and replication, which serve no
//! caller. A read that the frontend or a daemon serves from a pack never reaches a node, so it
//! is not counted anywhere here.
//!
//! One [`LegacyReads`] per node, shared by its service and by the `/metrics` scrape of the node
//! ([`LegacyReads::render`]). It is created by the process that starts the node, so a process
//! that is not a storage node renders nothing.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use kbf_meta::{ActionRecord, Role};
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::segments_server::Segments;
use kbf_types::Digest;

use crate::service::OpenWrites;
use crate::{ActionEntry, Answer, PutStream, ReadStream, Storage};

/// A way a storage node serves a read from its own copy: one row of
/// `kbf_storage_legacy_read_total`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Path {
    /// ByteStream `Read` of a blob.
    CasByteStream,
    /// `Segments` `ReadRange`.
    CasSegments,
    /// A blob read by the daemon half of the same process, without a connection.
    CasInProcess,
    /// `ActionStore` `Get` of an entry.
    AcActionStore,
    /// An action-cache entry read by the daemon half of the same process.
    AcInProcess,
}

impl Path {
    /// Every path, in the order they are rendered.
    pub const ALL: [Self; 5] = [
        Self::CasByteStream,
        Self::CasSegments,
        Self::CasInProcess,
        Self::AcActionStore,
        Self::AcInProcess,
    ];

    /// The `kind` label: what was read.
    #[must_use]
    pub const fn kind(self) -> &'static str {
        match self {
            Self::CasByteStream | Self::CasSegments | Self::CasInProcess => "cas",
            Self::AcActionStore | Self::AcInProcess => "ac",
        }
    }

    /// The `source` label: which call served it.
    #[must_use]
    pub const fn source(self) -> &'static str {
        match self {
            Self::CasByteStream => "bytestream",
            Self::CasSegments => "segments",
            Self::AcActionStore => "action_store",
            Self::CasInProcess | Self::AcInProcess => "in_process",
        }
    }
}

/// The reads a storage node has served from its own copy since it started, by [`Path`].
#[derive(Debug, Default)]
pub struct LegacyReads {
    counts: [AtomicU64; Path::ALL.len()],
}

impl LegacyReads {
    /// Counts one read served by `path`.
    pub fn count(&self, path: Path) {
        self.counts[path as usize].fetch_add(1, Ordering::Relaxed);
    }

    /// The reads served by `path`.
    #[must_use]
    pub fn get(&self, path: Path) -> u64 {
        self.counts[path as usize].load(Ordering::Relaxed)
    }

    /// The reads served by every path.
    #[must_use]
    pub fn total(&self) -> u64 {
        Path::ALL.iter().map(|path| self.get(*path)).sum()
    }

    /// Appends `kbf_storage_legacy_read_total` to `out` in the Prometheus text format: one
    /// row for each path, 0 included, so the series exists before the first read.
    pub fn render(&self, out: &mut String) {
        let name = "kbf_storage_legacy_read_total";
        let _ = writeln!(
            out,
            "# HELP {name} Reads this storage node served from its own copy of the data, by what \
             was read and the call that served it. A read of a blob that is not found, of the \
             empty blob, and the node's own reads for flushing and repair are not counted.\n\
             # TYPE {name} counter"
        );
        for path in Path::ALL {
            let _ = writeln!(
                out,
                "{name}{{kind=\"{}\",source=\"{}\"}} {}",
                path.kind(),
                path.source(),
                self.get(path)
            );
        }
    }
}

/// A node's [`Storage`] as the daemon half of a `storage-and-daemon` process without a
/// frontend reads it: directly, with no service in between (the farm is this node alone, a
/// map of itself). Every call is the inner storage's; a blob read that is found and an
/// action entry that is found are counted on the `in_process` rows, as
/// [`crate::service::InProcess`] counts the same reads on a node that registers with a
/// frontend.
pub struct CountedStorage {
    inner: Arc<dyn Storage>,
    reads: Arc<LegacyReads>,
}

impl CountedStorage {
    /// `inner`, counting into `reads` (the node's, so its scrape shows them).
    #[must_use]
    pub fn new(inner: Arc<dyn Storage>, reads: Arc<LegacyReads>) -> Self {
        Self { inner, reads }
    }

    /// The empty blob is held by no store, so reading it is not a read of the node's copy.
    fn count_blob(&self, digest: &Digest) {
        if digest.size_bytes != 0 {
            self.reads.count(Path::CasInProcess);
        }
    }
}

impl Storage for CountedStorage {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        self.inner.find_missing(digests)
    }

    fn find_durable<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        self.inner.find_durable(digests)
    }

    fn find_durable_waiting<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        self.inner.find_durable_waiting(digests)
    }

    fn read_blob<'a>(&'a self, digest: &'a Digest) -> Answer<'a, Bytes> {
        Box::pin(async move {
            let blob = self.inner.read_blob(digest).await?;
            self.count_blob(digest);
            Ok(blob)
        })
    }

    fn read_range<'a>(
        &'a self,
        digest: &'a Digest,
        offset: u64,
        limit: u64,
    ) -> Answer<'a, ReadStream> {
        Box::pin(async move {
            let stream = self.inner.read_range(digest, offset, limit).await?;
            self.count_blob(digest);
            Ok(stream)
        })
    }

    fn put(&self, bytes: Bytes) -> Answer<'_, Digest> {
        self.inner.put(bytes)
    }

    fn put_unowned(&self, bytes: Bytes) -> Answer<'_, Digest> {
        self.inner.put_unowned(bytes)
    }

    fn stream(&self, size: u64) -> Option<Box<dyn PutStream>> {
        self.inner.stream(size)
    }

    fn put_streamed(
        &self,
        size: u64,
        bytes: BoxStream<'static, std::io::Result<Bytes>>,
        claim: BoxFuture<'static, Option<Digest>>,
    ) -> Answer<'_, Option<Digest>> {
        self.inner.put_streamed(size, bytes, claim)
    }

    fn segments(&self) -> Arc<dyn Segments> {
        self.inner.segments()
    }

    fn segments_counted(&self, open: &OpenWrites) -> Arc<dyn Segments> {
        self.inner.segments_counted(open)
    }

    fn action_result<'a>(
        &'a self,
        action: &'a Digest,
    ) -> Answer<'a, Option<(Digest, ActionResult)>> {
        Box::pin(async move {
            let hit = self.inner.action_result(action).await?;
            if hit.is_some() {
                self.reads.count(Path::AcInProcess);
            }
            Ok(hit)
        })
    }

    fn action_entry<'a>(&'a self, action: &'a Digest) -> Answer<'a, Option<ActionEntry>> {
        Box::pin(async move {
            let hit = self.inner.action_entry(action).await?;
            if hit.is_some() {
                self.reads.count(Path::AcInProcess);
            }
            Ok(hit)
        })
    }

    fn prepare_action_result<'a>(&'a self, result: &'a ActionResult) -> Answer<'a, ActionRecord> {
        self.inner.prepare_action_result(result)
    }

    fn commit_action_record(
        &self,
        role: Role,
        action: Digest,
        record: ActionRecord,
    ) -> Answer<'_, ()> {
        self.inner.commit_action_record(role, action, record)
    }

    fn ram_ack(&self) -> bool {
        self.inner.ram_ack()
    }

    fn unconfirmed(&self, blobs: usize) {
        self.inner.unconfirmed(blobs);
    }
}

#[cfg(test)]
mod tests;
