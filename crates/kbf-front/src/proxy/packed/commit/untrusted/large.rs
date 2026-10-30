//! A large object of a run: [`Large`] streams the raw object a task wrote under an `L<i>` key
//! from the object store, through SHA-256, into the CAS, a window at a time.
//!
//! The object is never held whole. The object store is read in windows of [`WINDOW_BYTES`], each
//! read only when the CAS write asks for the next piece, so this process holds one window (and the
//! write's own piece in flight) however large the object is. The bytes are hashed as they pass and
//! the write names the digest the manifest gave, so a store of the blob keeps nothing under that
//! digest unless the bytes are its own: this module says which of the two refused (the bytes, or the
//! store) from what it hashed, whatever error the store answered.

use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream;
use kbf_objstore::{ByteRange, ObjectKey};
use kbf_segments::RecordHasher;
use kbf_storage::PieceStream;
use kbf_types::Digest;

use super::task::{RejectReason, Step, Stop, unavailable};
use super::{IssuedPackKey, Packs};
use crate::backend::Backend;

/// The most bytes of a large object read from the object store at once.
pub(super) const WINDOW_BYTES: u64 = 8 << 20;

/// How a pass over the object ended.
enum Ended {
    /// Every byte was read and they hash to this.
    Read(Digest),
    /// The read stopped, for this reason.
    Stopped(Stop),
}

/// What a pass over the object has seen.
#[derive(Default)]
struct Pass {
    hasher: RecordHasher,
    ended: Option<Ended>,
}

fn locked(pass: &Mutex<Pass>) -> MutexGuard<'_, Pass> {
    pass.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One large object of a run: the object under its key, and the blob the manifest says it is.
pub(super) struct Large<'a> {
    packs: &'a Packs,
    object: ObjectKey,
    digest: Digest,
    pass: Arc<Mutex<Pass>>,
}

impl<'a> Large<'a> {
    /// The object under `key`, which the manifest says is the blob `digest`.
    pub(super) fn new(packs: &'a Packs, key: &IssuedPackKey, digest: Digest) -> Self {
        Self {
            packs,
            object: key.object.clone(),
            digest,
            pass: Arc::default(),
        }
    }

    /// Checks the object's length: it must be there and as long as the digest says. An object of
    /// another length is not that blob (`HashMismatch`), found before a byte is read.
    pub(super) async fn check_length(&self) -> Step<()> {
        match self.packs.store.len(&self.object).await {
            Ok(Some(len)) if len == self.digest.size_bytes => Ok(()),
            Ok(Some(_)) => Err(RejectReason::HashMismatch.into()),
            Ok(None) => Err(RejectReason::Incomplete.into()),
            Err(error) => Err(unavailable(&format!(
                "the length of {}: {error}",
                self.object
            ))),
        }
    }

    /// Streams the object into the CAS through `backend`.
    ///
    /// The object store is read again from the first byte if the write has to be made again (the
    /// next owner of the blob, a newer map).
    ///
    /// # Errors
    /// `HashMismatch` if the bytes (all of them, or the last window cut short) are not the blob,
    /// and nothing is kept under its digest; `Incomplete` if the object is gone; `Unavailable` if
    /// the object store or the CAS failed.
    pub(super) async fn store(&self, backend: &Backend) -> Step<()> {
        let open = || self.open();
        let stored = backend.put_streamed(&self.digest, &open).await;
        let ended = locked(&self.pass).ended.take();
        match (ended, stored) {
            (Some(Ended::Read(actual)), stored) if actual == self.digest => {
                stored.map_err(|error| unavailable(&format!("storing {}: {error}", self.digest)))
            }
            (Some(Ended::Read(_)), _) => Err(RejectReason::HashMismatch.into()),
            (Some(Ended::Stopped(stop)), _) => Err(stop),
            (None, stored) => Err(unavailable(&format!(
                "the CAS took no bytes of {}: {stored:?}",
                self.object
            ))),
        }
    }

    /// The pieces of the object from its first byte. Starts the pass over.
    fn open(&self) -> PieceStream {
        *locked(&self.pass) = Pass::default();
        let reading = Reading {
            packs: self.packs.clone(),
            object: self.object.clone(),
            size: self.digest.size_bytes,
            at: 0,
            pass: Arc::clone(&self.pass),
        };
        stream::try_unfold(reading, Reading::step).boxed()
    }
}

/// The state of the stream of one pass: where the next window starts.
struct Reading {
    packs: Packs,
    object: ObjectKey,
    size: u64,
    at: u64,
    pass: Arc<Mutex<Pass>>,
}

impl Reading {
    /// The next window, or the end of the object (which says what its bytes hash to).
    async fn step(mut self) -> io::Result<Option<(Bytes, Self)>> {
        if self.at == self.size {
            let mut pass = locked(&self.pass);
            let (seen, _) = std::mem::take(&mut pass.hasher).finish();
            pass.ended = Some(Ended::Read(seen));
            return Ok(None);
        }
        let want = WINDOW_BYTES.min(self.size - self.at);
        let range = ByteRange::new(self.at, want).expect("a window has bytes");
        match self.packs.fetch(&self.object, range).await {
            Ok(bytes) if bytes.len() as u64 == want => {
                locked(&self.pass).hasher.update(&bytes);
                self.at += want;
                Ok(Some((bytes, self)))
            }
            Ok(_) => Err(self.stopped(
                RejectReason::HashMismatch.into(),
                "the object is shorter than its length said",
            )),
            Err(error) => {
                let stop = Stop::from(error).absent_is_incomplete();
                Err(self.stopped(stop, "reading the object failed"))
            }
        }
    }

    /// Notes that the pass stopped for `stop`, and the error that ends the stream.
    fn stopped(&self, stop: Stop, why: &str) -> io::Error {
        locked(&self.pass).ended = Some(Ended::Stopped(stop));
        io::Error::other(format!("{}: {why}", self.object))
    }
}
