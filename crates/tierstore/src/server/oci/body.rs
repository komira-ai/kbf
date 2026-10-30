//! The bodies of the front's answers, and the stream that reads a blob out of the store.

use std::sync::Arc;

use bytes::Bytes;
use futures::stream;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt as _, Empty, Full, StreamBody};
use hyper::body::Frame;
use sha2::{Digest as _, Sha256};

use super::metrics::OciMetrics;
use super::names::Digest;
use crate::{Backend, ByteRange};

/// Why a body ended in an error. The connection is closed without the rest of the answer, which
/// is how the client is told the bytes it was sent are not all there.
pub type BodyError = Box<dyn std::error::Error + Send + Sync>;

/// The body of every answer.
pub type OciBody = UnsyncBoxBody<Bytes, BodyError>;

/// The most bytes read from the store at once for a blob.
pub(super) const CHUNK_BYTES: u64 = 4 << 20;

/// A body of `bytes`.
pub(super) fn full(bytes: Bytes) -> OciBody {
    Full::new(bytes)
        .map_err(|never| match never {})
        .boxed_unsync()
}

/// No body.
pub(super) fn empty() -> OciBody {
    Empty::new().map_err(|never| match never {}).boxed_unsync()
}

/// A blob's bytes `start..end` (end excluded), read from the store a chunk at a time.
pub(super) struct BlobRead {
    pub(super) backend: Arc<dyn Backend>,
    pub(super) key: String,
    pub(super) start: u64,
    pub(super) end: u64,
    /// The digest to check the bytes against: set only for a read of the whole object, since a
    /// part of an object has no digest of its own.
    pub(super) verify: Option<Digest>,
    pub(super) metrics: Arc<OciMetrics>,
    /// The most bytes asked of the store at once.
    pub(super) chunk: u64,
}

/// Where a [`BlobRead`] is.
struct Reading {
    read: BlobRead,
    offset: u64,
    hash: Sha256,
    done: bool,
}

impl BlobRead {
    /// The body that reads the bytes.
    ///
    /// A whole-object read hashes what it reads and compares the hash with the digest before
    /// it sends the last chunk: if they differ the body ends in an error instead of delivering
    /// that chunk, so the response, which has already said how long it is, is cut short and no
    /// client receives all of its length. (Hyper treats a message as complete once its declared
    /// length is written, so a check after the last byte would come too late.) A store that
    /// gives another number of bytes than it was asked for, or fails, ends the body in an error
    /// the same way.
    pub(super) fn body(self) -> OciBody {
        let state = Reading {
            offset: self.start,
            read: self,
            hash: Sha256::new(),
            done: false,
        };
        let frames = stream::unfold(state, |state| async move { state.next().await });
        StreamBody::new(frames).boxed_unsync()
    }
}

impl Reading {
    /// The next frame, and the state after it; `None` once the body has ended.
    async fn next(mut self) -> Option<(Result<Frame<Bytes>, BodyError>, Self)> {
        if self.done {
            return None;
        }
        let mut chunk = None;
        if self.offset < self.read.end {
            let want = (self.read.end - self.offset).min(self.read.chunk);
            let range = ByteRange::new(self.offset, want);
            let got = self.read.backend.get(&self.read.key, Some(range)).await;
            let read = got.map_err(|error| error.to_string());
            let read = read.and_then(|bytes| match bytes.len() as u64 == want {
                true => Ok(bytes),
                false => Err(format!(
                    "the store gave {} bytes of the {want} asked for",
                    bytes.len()
                )),
            });
            match read {
                Ok(bytes) => {
                    self.hash.update(&bytes);
                    self.offset += want;
                    chunk = Some(bytes);
                }
                Err(why) => {
                    self.done = true;
                    let why = format!("{}: {why}", self.read.key);
                    eprintln!("tierstore: oci: {why}");
                    return Some((Err(why.into()), self));
                }
            }
        }
        if self.offset == self.read.end {
            self.done = true;
            if let Err(error) = self.finish() {
                return Some((Err(error), self));
            }
        }
        chunk.map(|bytes| {
            self.read.metrics.sent(bytes.len() as u64);
            (Ok(Frame::data(bytes)), self)
        })
    }

    /// Checks the bytes read against the digest, if the whole object was read.
    fn finish(&self) -> Result<(), BodyError> {
        let Some(digest) = &self.read.verify else {
            return Ok(());
        };
        let found = hex::encode(self.hash.clone().finalize());
        if found == digest.hex() {
            return Ok(());
        }
        self.read.metrics.mismatch();
        let key = &self.read.key;
        let why = format!("{key} does not hold the bytes of its digest (they hash to {found})");
        eprintln!("tierstore: oci: {why}");
        Err(why.into())
    }
}
