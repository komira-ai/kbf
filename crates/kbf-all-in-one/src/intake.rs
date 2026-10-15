//! [`Taking`]: the in-process plane's [`Intake`], which takes each output file's bytes as
//! the walk reads them to hash them, so a file is read once.
//!
//! The walk hands each chunk it reads to the plane ([`IntakeFile::write`]) as it hashes
//! it, holding one chunk itself; the plane keeps the file's bytes, and when the walk
//! finishes the file (its digest known), they become a [`Record`] of the plane's batch,
//! held in memory ([`Source::Bytes`], no copy). A batch of [`SEGMENT_UPLOAD_BYTES`] or
//! [`MAX_UPLOAD_RECORDS`] goes to [`Cas::put_records`] at once, on the server half's
//! runtime, while the walk goes on: FindMissing for its digests, then the uploads the
//! transfer engine would plan for the missing ones (`Begin`, `PutRange`, `Commit`), whose
//! bytes are those buffers. No more than [`DEFAULT_SEGMENT_UPLOADS`] batches are in
//! flight; a walk that finishes a file past that waits for one to end. Two files with one
//! digest are stored once. [`Intake::finish`] sends the rest and waits for every batch.
//!
//! What this holds in memory: the files being read (each up to its size, at most one per
//! hashing thread), the batch being filled, and the batches in flight. A file larger than
//! the server's largest blob is not taken: it goes to `put_records` as a file the walk
//! hashed, as any output does on the remote plane, which the server refuses.

use std::collections::BTreeSet;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_transfer::{
    Cas, CasError, DEFAULT_SEGMENT_UPLOADS, Intake, IntakeFile, MAX_UPLOAD_RECORDS, Record,
    SEGMENT_UPLOAD_BYTES, Source,
};
use tokio::task::JoinHandle;

use crate::LocalDataPlane;

/// The largest file taken: the largest blob the server accepts
/// (`kbf_front::MAX_BLOB_BYTES`).
pub(crate) const LARGEST_TAKEN: u64 = 1 << 30;

/// One walk's intake into a [`LocalDataPlane`].
pub(crate) struct Taking(Arc<Shared>);

struct Shared {
    plane: LocalDataPlane,
    /// A batch of this many bytes is sent ([`SEGMENT_UPLOAD_BYTES`]).
    batch_bytes: u64,
    /// The most batches in flight ([`DEFAULT_SEGMENT_UPLOADS`]).
    uploads: usize,
    state: Mutex<State>,
    /// Signalled whenever a batch's upload ends.
    ended: Condvar,
}

#[derive(Default)]
struct State {
    /// Files finished and not yet sent.
    batch: Vec<Record>,
    /// Their bytes.
    bytes: u64,
    /// Every digest taken: a second file with one is not stored again.
    seen: BTreeSet<kbf_types::Digest>,
    /// Batches sent whose upload has not ended.
    in_flight: usize,
    uploads: Vec<JoinHandle<Result<(), CasError>>>,
}

impl State {
    /// The batch so far, counted in flight.
    fn take(&mut self) -> Vec<Record> {
        self.bytes = 0;
        self.in_flight += 1;
        std::mem::take(&mut self.batch)
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Taking {
    /// The intake of a walk into `plane`.
    pub(crate) fn new(plane: LocalDataPlane) -> Self {
        Self::with(plane, SEGMENT_UPLOAD_BYTES, DEFAULT_SEGMENT_UPLOADS)
    }

    /// [`Taking::new`], sending a batch of `batch_bytes` with at most `uploads` in flight.
    fn with(plane: LocalDataPlane, batch_bytes: u64, uploads: usize) -> Self {
        Self(Arc::new(Shared {
            plane,
            batch_bytes,
            uploads,
            state: Mutex::default(),
            ended: Condvar::new(),
        }))
    }
}

impl Shared {
    /// Adds a finished file to the batch, and sends the batch once it is full, after
    /// waiting for room among the batches in flight.
    fn add(self: &Arc<Self>, record: Record) {
        let batch = {
            let mut state = lock(&self.state);
            if !state.seen.insert(record.digest) {
                return;
            }
            state.bytes += record.digest.size_bytes;
            state.batch.push(record);
            // Both parts are looked at, whichever is true: no short-circuit to cover.
            let full =
                (state.bytes >= self.batch_bytes) | (state.batch.len() >= MAX_UPLOAD_RECORDS);
            if !full {
                return;
            }
            while state.in_flight >= self.uploads {
                state = self
                    .ended
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            state.take()
        };
        self.send(batch);
    }

    /// Stores `batch` on the server half's runtime. Called with the state unlocked: a
    /// runtime that is gone drops the upload, and its [`Ended`] with it, at once.
    fn send(self: &Arc<Self>, batch: Vec<Record>) {
        let (plane, ended) = (self.plane.clone(), Ended(Arc::clone(self)));
        let upload = self.plane.server().spawn(async move {
            let _ended = ended;
            plane.put_records(batch).await
        });
        lock(&self.state).uploads.push(upload);
    }
}

/// Counts a batch's upload ended when it is dropped, whether it ran or not.
struct Ended(Arc<Shared>);

impl Drop for Ended {
    fn drop(&mut self) {
        lock(&self.0.state).in_flight -= 1;
        self.0.ended.notify_all();
    }
}

impl Intake for Taking {
    fn file(&self, size: u64) -> Option<Box<dyn IntakeFile>> {
        (size <= LARGEST_TAKEN).then(|| {
            Box::new(TakenFile {
                bytes: Vec::with_capacity(usize::try_from(size).unwrap_or(0)),
                shared: Arc::clone(&self.0),
            }) as Box<dyn IntakeFile>
        })
    }

    fn finish(&self) -> BoxFuture<'_, Result<(), CasError>> {
        Box::pin(async move {
            let batch = {
                let mut state = lock(&self.0.state);
                (!state.batch.is_empty()).then(|| state.take())
            };
            if let Some(batch) = batch {
                self.0.send(batch);
            }
            let uploads = std::mem::take(&mut lock(&self.0.state).uploads);
            let mut answer = Ok(());
            for upload in uploads {
                let done = upload.await.unwrap_or_else(|gone| {
                    Err(CasError::Unavailable(
                        "outputs".to_owned(),
                        format!("the server half did not answer: {gone}"),
                    ))
                });
                answer = answer.and(done);
            }
            answer
        })
    }
}

/// One file's bytes, as the walk reads them.
struct TakenFile {
    bytes: Vec<u8>,
    shared: Arc<Shared>,
}

impl IntakeFile for TakenFile {
    fn write(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
    }

    fn finish(self: Box<Self>, digest: kbf_types::Digest, crc32c: u32) {
        let Self { bytes, shared } = *self;
        shared.add(Record {
            digest,
            crc32c,
            source: Source::Bytes(Bytes::from(bytes)),
        });
    }
}

#[cfg(test)]
#[path = "intake_tests.rs"]
mod tests;
