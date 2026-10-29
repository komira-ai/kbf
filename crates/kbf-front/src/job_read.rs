//! The job-facing read: how a task on a worker fetches the inputs of its one Action.
//!
//! A task holds a read grant ([`crate::grants`]), not a client certificate, so it cannot use
//! the REAPI listener. The job-facing listener of `kbf frontend` (`--job-listen`) answers it
//! over HTTPS, and this module is what answers: `GET /<hex>/<size>` with
//! `Authorization: Bearer <grant>`, the form `kbf-task` requests, and nothing else. Every
//! other path is 404 and every other method 405.
//!
//! A request is served only when the grant verifies (a forged, garbled, expired or revoked
//! one is 401), and the digest asked for is in the grant's scope (403): the Action the grant
//! is for, its Command, and the Directory blobs and files of the Action's input tree. The
//! scope is worked out from the blobs themselves, by reading the Action and walking the tree
//! from its root, once per Action, and remembered ([`Limits::scopes`] Actions at most). A
//! grant never reads a blob outside it, so it cannot read another tenant's bytes.
//!
//! The bytes come from a [`BlobSource`], which is the frontend's [`Backend`] in a running
//! server: the pack index first, then the storage nodes, each read checked against its
//! digest, as a ByteStream read is. The blob is held whole while it is sent, as that read
//! holds it, so a response is never larger than [`MAX_BLOB_BYTES`] (413 above it, before
//! anything is read), and at most [`Limits::reads_in_flight`] reads are open at once; the
//! others wait for one to end.
//!
//! An answer carries a reason code in its body and no other detail: `missing_token`,
//! `invalid_token`, `expired`, `revoked`, `no_scope`, `out_of_scope`, `too_large`,
//! `not_found`, `method_not_allowed` or `store_unavailable`. Requests by outcome and the
//! bytes served are counted ([`JobRead::render_metrics`]).

use std::sync::Arc;

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_storage::StorageError;
use kbf_types::Digest;
use tonic::codegen::http::{Method, StatusCode};

use crate::backend::Backend;
use crate::grants::Grants;
use crate::proxy::MAX_UNTRUSTED_BLOB_BYTES;

/// The most bytes one response carries: the per-blob cap of the packs a task's results are
/// written into.
pub const MAX_BLOB_BYTES: u64 = MAX_UNTRUSTED_BLOB_BYTES;

/// Where the blobs a job reads come from.
///
/// An implementation returns the bytes that hash to `digest`, or [`StorageError::NotFound`]
/// when it holds none.
pub trait BlobSource: Send + Sync {
    /// The blob `digest`, read whole and checked against its digest.
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Result<Bytes, StorageError>>;
}

impl BlobSource for Backend {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Result<Bytes, StorageError>> {
        Box::pin(Backend::read_blob(self, digest))
    }
}

/// The bounds of a [`JobRead`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The reads of blobs open at once (at least 1); a request over it waits for one to end.
    pub reads_in_flight: usize,
    /// The most digests a scope may hold: an input tree past it gives its Action no scope.
    pub scope_digests: usize,
    /// The Actions whose scope is remembered; at the bound the memory is emptied and begins
    /// again.
    pub scopes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            reads_in_flight: 32,
            scope_digests: 1 << 20,
            scopes: 256,
        }
    }
}

/// What the listener answers a request with.
#[derive(Debug)]
pub struct Reply {
    /// The status.
    pub status: StatusCode,
    /// The blob on 200, else the reason code and a newline.
    pub body: Bytes,
}

/// The job-facing read of one frontend. Shared by every connection.
pub struct JobRead {
    _source: Arc<dyn BlobSource>,
    _grants: Arc<Grants>,
    _limits: Limits,
}

impl std::fmt::Debug for JobRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobRead").finish_non_exhaustive()
    }
}

impl JobRead {
    /// The read of `source` for the grants `grants` issued, within `limits`.
    #[must_use]
    pub fn new(source: Arc<dyn BlobSource>, grants: Arc<Grants>, limits: Limits) -> Self {
        Self {
            _source: source,
            _grants: grants,
            _limits: limits,
        }
    }

    /// The answer to `method` `path` with the `Authorization` header `authorization`.
    /// `path` is the request's path alone, without its query.
    pub async fn respond(&self, method: &Method, path: &str, authorization: Option<&str>) -> Reply {
        let _ = (method, path, authorization);
        Reply {
            status: StatusCode::NOT_IMPLEMENTED,
            body: Bytes::new(),
        }
    }

    /// Appends the counters to `out`: `kbf_job_read_requests_total` by `outcome` and
    /// `kbf_job_read_bytes_total`.
    pub fn render_metrics(&self, out: &mut String) {
        let _ = out;
    }

    /// [`JobRead::render_metrics`] when there is a job-facing read, and nothing when there
    /// is none, so that a scrape has one branch for it.
    pub fn render_metrics_of(read: Option<&Self>, out: &mut String) {
        if let Some(read) = read {
            read.render_metrics(out);
        }
    }
}
