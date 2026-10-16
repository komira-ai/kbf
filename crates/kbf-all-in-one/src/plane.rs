//! [`LocalDataPlane`]: the daemon's data plane ([`kbf_transfer::Cas`]) as calls into the
//! server half of the same process.
//!
//! Every call runs on the server half's async runtime, through the server's
//! [`LocalStore`]: the same cache and the same `Segments` handlers a remote daemon's calls
//! reach through the worker listener, with no TLS, no protobuf encoding and no socket
//! between. What a remote daemon sends is unchanged; this plane sends nothing.
//!
//! - **Output files, as the walk reads them** ([`Cas::intake`], `crate::intake`): the
//!   container driver's walk hands each file's chunks to the plane as it hashes them, so
//!   each output file (and stdout and stderr) is read once; the plane stores them as
//!   below, from those bytes, in batches while the walk goes on.
//! - **Outputs** ([`Cas::put_records`], and [`Cas::put_spool`] through it): the walk
//!   hashed each file once. The plane asks the cache which digests it lacks, packs those
//!   records into segments and uploads exactly as the transfer engine plans them
//!   ([`kbf_transfer::plan`]), and hands each range of the records' bytes, read once from
//!   where the walk left them, to `Begin`, `PutRange` and `Commit`. Nothing is spooled
//!   and nothing is hashed here: with a write tier the server trusts the digests at the
//!   commit and its flusher hashes every record before the PUT, dropping one that does
//!   not match (#656); without one, `PutRange` hashes each record as its bytes arrive.
//! - **A whole blob** ([`Cas::put`], and [`Cas::put_chunks`] through it): hashed once,
//!   by the server's [`LocalStore::put`], under the admission a ByteStream `Write` takes.
//! - **Reads** ([`Cas::get`], and [`Cas::read_to`] through it): the cache's own read,
//!   which checks the bytes against the digest.
//! - **FindMissing** ([`Cas::missing`]): the cache's own answer. A call that fails
//!   answers every digest missing, as the transfer engine's does.
//!
//! A server half that is gone (its runtime shut down) fails each call UNAVAILABLE.

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;

use futures::{StreamExt as _, TryStreamExt as _, future, stream};
use kbf_front::{MAX_UNSTAGED_BLOB_BYTES, unstaged_refusal};
use kbf_proto::reapi::Digest;
use kbf_proto::worker::segments_server::Segments;
use kbf_proto::worker::{BeginRequest, CommitRequest, PutRangeRequest, UploadSegment};
use kbf_segments::MAX_SEGMENT_BYTES;
use kbf_server::LocalStore;
use kbf_transfer::{
    Cas, CasError, Chunks, DEFAULT_SEGMENT_UPLOADS, Intake, MAX_RANGE_BYTES, MAX_UPLOAD_RECORDS,
    Planned, Record, SEGMENT_UPLOAD_BYTES, label, plan, proto_digest, read_pieces,
};
use kbf_types::{DigestFunction, LeaseId};
use tokio::runtime::Handle;
use tonic::{Code, Request, Status};

use crate::intake::Taking;

/// The daemon's data plane as calls into the server half of this process. Clones share
/// the server half's store and this plane's own `Segments` service.
#[derive(Clone)]
pub struct LocalDataPlane {
    store: LocalStore,
    /// This plane's own `Segments` service: its uploads, no one else's.
    segments: Arc<dyn Segments>,
    /// The server half's runtime, which every call runs on.
    server: Handle,
}

impl LocalDataPlane {
    /// The plane over the server's `store`, its calls run on the server half's runtime
    /// `server`.
    #[must_use]
    pub fn new(store: LocalStore, server: Handle) -> Self {
        let segments = store.segments();
        Self {
            store,
            segments,
            server,
        }
    }

    /// The server half's runtime.
    pub(crate) fn server(&self) -> &Handle {
        &self.server
    }

    /// The server half's store.
    pub(crate) fn store(&self) -> &LocalStore {
        &self.store
    }

    /// Runs `call` on the server half's runtime and returns its answer; UNAVAILABLE when
    /// that runtime is gone.
    async fn call<T: Send + 'static>(
        &self,
        call: impl Future<Output = Result<T, Status>> + Send + 'static,
    ) -> Result<T, Status> {
        match self.server.spawn(call).await {
            Ok(answer) => answer,
            Err(gone) => Err(Status::unavailable(format!(
                "the server half did not answer: {gone}"
            ))),
        }
    }

    /// Which of `digests` the cache does not hold, in the order asked.
    async fn absent(
        &self,
        digests: Vec<kbf_types::Digest>,
    ) -> Result<Vec<kbf_types::Digest>, Status> {
        let store = self.store.clone();
        self.call(async move { store.find_missing(&digests).await.map_err(Status::from) })
            .await
    }

    /// One upload: `Begin` with the segments' footers, each range of the records'
    /// bytes read from their sources, and `Commit`, which answers once the records are
    /// staged (or stored) and indexed.
    async fn upload(&self, upload: Vec<Planned>) -> Result<(), CasError> {
        let begin = BeginRequest {
            segments: upload
                .iter()
                .map(|s| UploadSegment {
                    length: s.length(),
                    footer: s.footer(),
                })
                .collect(),
        };
        let segments = Arc::clone(&self.segments);
        let begun = self.call(async move { segments.begin(Request::new(begin)).await });
        let id = begun.await.map_err(not_stored)?.into_inner().upload;
        for (index, segment) in upload.iter().enumerate() {
            for (start, end) in segment.ranges(MAX_RANGE_BYTES) {
                let pieces = segment.pieces(start, end);
                let bytes = read_pieces(pieces, end - start).await.map_err(not_read)?;
                let request = PutRangeRequest {
                    upload: id,
                    segment: u32::try_from(index).unwrap_or(u32::MAX),
                    offset: start,
                    bytes,
                };
                let segments = Arc::clone(&self.segments);
                let put = self.call(async move { segments.put_range(Request::new(request)).await });
                put.await.map_err(not_stored)?;
            }
        }
        let segments = Arc::clone(&self.segments);
        let commit = CommitRequest { upload: id };
        let committed = self.call(async move { segments.commit(Request::new(commit)).await });
        committed.await.map(drop).map_err(not_stored)
    }
}

/// `digest` as the server's digest; `None` when it is not a SHA-256 digest with a size,
/// which no store holds.
fn parse(digest: &Digest) -> Option<kbf_types::Digest> {
    kbf_types::Digest::parse(DigestFunction::Sha256, &label(digest)).ok()
}

/// An upload of outputs the server refused or did not answer.
fn not_stored(status: Status) -> CasError {
    CasError::Unavailable("outputs".to_owned(), status.message().to_owned())
}

/// Outputs whose bytes could not be read, or are no longer the ones the walk hashed.
fn not_read(e: std::io::Error) -> CasError {
    CasError::Read("outputs".to_owned(), e.to_string())
}

impl Cas for LocalDataPlane {
    type Lease = Arc<Self>;

    fn lease(self: &Arc<Self>, _: &LeaseId) -> Arc<Self> {
        Arc::clone(self)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        let blob = label(digest);
        let Some(parsed) = parse(digest) else {
            return Err(CasError::Missing(blob));
        };
        let store = self.store.clone();
        let read = self.call(async move { store.read_blob(&parsed).await.map_err(Status::from) });
        match read.await {
            Ok(bytes) => Ok(bytes),
            Err(status) if status.code() == Code::NotFound => Err(CasError::Missing(blob)),
            Err(status) => Err(CasError::Unavailable(blob, status.message().to_owned())),
        }
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        let blob = format!("of {} bytes", bytes.len());
        let store = self.store.clone();
        let stored = self.call(async move { store.put(bytes).await.map_err(Status::from) });
        match stored.await {
            Ok(digest) => Ok(proto_digest(&digest)),
            Err(status) => Err(CasError::Unavailable(blob, status.message().to_owned())),
        }
    }

    /// A blob over [`MAX_UNSTAGED_BLOB_BYTES`] is refused before a chunk is read: it
    /// would be gathered whole in memory (a log the intake did not take, for want of
    /// room on a write tier). A smaller one is gathered and stored by [`Cas::put`].
    async fn put_chunks(&self, digest: Digest, mut chunks: Chunks) -> Result<Digest, CasError> {
        let blob = label(&digest);
        if u64::try_from(digest.size_bytes).unwrap_or(u64::MAX) > MAX_UNSTAGED_BLOB_BYTES {
            let why = unstaged_refusal(&blob);
            return Err(CasError::Unavailable(blob, why));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(|e| CasError::Read(blob.clone(), e.to_string()))?;
            bytes.extend_from_slice(&chunk);
        }
        let stored = self.put(bytes).await?;
        if stored != digest {
            return Err(CasError::Corrupt(blob, label(&stored)));
        }
        Ok(stored)
    }

    async fn missing(&self, digests: &[Digest]) -> Vec<Digest> {
        // A digest that does not parse is held by no store: ask nothing, as a
        // FindMissing the server refuses answers every digest missing.
        let Some(parsed) = digests.iter().map(parse).collect::<Option<Vec<_>>>() else {
            return digests.to_vec();
        };
        match self.absent(parsed).await {
            Ok(absent) => absent.iter().map(proto_digest).collect(),
            Err(_) => digests.to_vec(),
        }
    }

    async fn put_records(&self, records: Vec<Record>) -> Result<(), CasError> {
        let asked: Vec<kbf_types::Digest> = records.iter().map(|r| r.digest).collect();
        // A FindMissing that fails sends every record, as the transfer engine does.
        let absent: BTreeSet<kbf_types::Digest> = self
            .absent(asked.clone())
            .await
            .unwrap_or(asked)
            .into_iter()
            .collect();
        let missing: Vec<Record> = records
            .into_iter()
            .filter(|r| absent.contains(&r.digest))
            .collect();
        let uploads = plan(
            &missing,
            MAX_SEGMENT_BYTES,
            SEGMENT_UPLOAD_BYTES,
            MAX_UPLOAD_RECORDS,
        );
        // The first failure stops the rest; the uploads committed before it stay.
        stream::iter(uploads)
            .map(|upload| self.upload(upload))
            .buffer_unordered(DEFAULT_SEGMENT_UPLOADS)
            .try_for_each(|()| future::ready(Ok(())))
            .await
    }

    fn intake(&self) -> Option<Arc<dyn Intake>> {
        Some(Arc::new(Taking::new(self.clone())))
    }
}

#[cfg(test)]
#[path = "plane_tests.rs"]
mod tests;
