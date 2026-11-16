//! The one-pass object being written while the outputs are collected (`--object-format
//! one-pass`, docs/design/one-pass-task-object.md).
//!
//! A [`Spool`] owns the open `pack.part` file as a [`kbf_task_pack::one_pass::ObjectWriter`].
//! Collection feeds it two ways, both under one lock so the object is one stream of whole blobs:
//!
//! - a blob held in memory (a Directory, a Tree, a file of at most one chunk, stdout) comes
//!   through [`Spool::put`], and is written only if its digest is new;
//! - a larger file comes through the [`Tee`] the spool is for `kbf-outputs`: its bytes are
//!   written as they are hashed ([`TeeFile::write`]) and it is indexed when its digest is
//!   known ([`TeeFile::end`]). The tee holds the lock from its open to its end, so no other
//!   blob is written between the bytes of one.
//!
//! [`Spool::finish`] then writes the manifest, the index and the trailer. A write the file
//! refused is remembered ([`Spool::write_failure`]) so the run fails as a put (exit 13), not as
//! an unreadable output.

use std::collections::HashMap;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex, PoisonError};

use kbf_outputs::{Tee, TeeFile};
use kbf_proto::reapi;
use kbf_task_pack::TaskManifest;
use kbf_task_pack::one_pass::{ObjectWriter, Streaming, Written};
use kbf_types::Digest;
use prost::Message as _;
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::digest;
use crate::files;
use crate::out_dir::{OBJECT_NAME, OutDirError};
use crate::outputs::{OutputLimits, collect_spooled};
use crate::task::Failure;

/// What the task writes (`--object-format`): the one-pass object, always. The flag is still
/// accepted, so that a frontend that sends it (every one since the one-pass object) and one that
/// does not both work; the packs format it once chose between is gone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum ObjectFormat {
    /// One object, written while the outputs are read.
    #[default]
    OnePass,
}

/// The bytes buffered for the file at one time.
const BUFFER_BYTES: usize = 1 << 20;

/// What the object's file is written through.
type Sink = BufWriter<Box<dyn Write + Send>>;

type Writer = ObjectWriter<Sink>;

/// The first error the file gave a write, shared with the files being teed.
type Remembered = Arc<StdMutex<Option<io::Error>>>;

/// Remembers `error` if it is the first, and returns it.
fn record(failure: &Remembered, error: io::Error) -> io::Error {
    let mut first = failure.lock().unwrap_or_else(PoisonError::into_inner);
    first.get_or_insert_with(|| io::Error::new(error.kind(), error.to_string()));
    error
}

/// Collects the outputs into `spool`: `paths` are the input root, the working directory and the
/// declared outputs, `logs` the stdout and stderr files. A write the object's file refused is a
/// put failure (exit 13), not an output that could not be read.
///
/// # Errors
/// A [`Failure`].
pub(crate) async fn collect(
    spool: &Arc<Spool>,
    paths: (&Path, &str, &[String]),
    logs: (&Path, &Path),
    limits: OutputLimits,
) -> Result<reapi::ActionResult, Failure> {
    let (root, working_directory, outputs) = paths;
    let collecting = collect_spooled(
        root,
        working_directory,
        outputs,
        logs.0,
        logs.1,
        limits,
        spool,
    );
    match (collecting.await, spool.write_failure()) {
        (Err(_), Some(source)) => Err(OutDirError::Write {
            name: OBJECT_NAME.to_owned(),
            source,
        }
        .into()),
        (done, _) => Ok(done?),
    }
}

/// The object being written.
pub struct Spool {
    writer: Arc<Mutex<Writer>>,
    failure: Remembered,
    trees: Arc<StdMutex<HashMap<Digest, Vec<u8>>>>,
}

impl Spool {
    /// A spool writing to `file`, which is empty. The file is only written, never read back: a
    /// put directory opens a staged file once, for the create that wrote it.
    #[must_use]
    pub fn new(file: Box<dyn Write + Send>) -> Arc<Self> {
        let sink = BufWriter::with_capacity(BUFFER_BYTES, file);
        Arc::new(Self {
            writer: Arc::new(Mutex::new(ObjectWriter::new(sink))),
            failure: Remembered::default(),
            trees: Arc::default(),
        })
    }

    /// The first error the file gave a write, if any.
    #[must_use]
    pub fn write_failure(&self) -> Option<io::Error> {
        let first = self.failure.lock().unwrap_or_else(PoisonError::into_inner);
        first
            .as_ref()
            .map(|error| io::Error::new(error.kind(), error.to_string()))
    }

    /// Writes `bytes`, the blob `digest` names, unless it is empty or already in the object.
    /// The file is written on the blocking pool.
    ///
    /// # Errors
    /// The file refused the bytes, or the digest is not a SHA-256 digest.
    pub async fn put(&self, digest: &reapi::Digest, bytes: Vec<u8>) -> io::Result<()> {
        let digest = digest::from_reapi(digest).map_err(io::Error::other)?;
        let (writer, failure) = (Arc::clone(&self.writer), Arc::clone(&self.failure));
        let done = tokio::task::spawn_blocking(move || {
            let mut writer = writer.blocking_lock();
            (writer.blob(&digest, &bytes).map(drop)).map_err(|error| record(&failure, error))
        })
        .await;
        done.map_err(io::Error::other)?
    }

    /// Keeps `bytes`, an output directory's Tree, for [`Spool::files`], which reads the Trees
    /// from here and not from the object's file. The Tree is written to the object by
    /// [`Spool::put`] as any blob is; this is the collector's own copy of a record it just made.
    pub fn keep_tree(&self, bytes: &[u8]) {
        let digest = kbf_segments::sha256(bytes);
        let mut trees = self.trees.lock().unwrap_or_else(PoisonError::into_inner);
        trees.entry(digest).or_insert_with(|| bytes.to_vec());
    }

    /// The first digest `manifest` names that the object does not hold, if any. The empty blob is
    /// implicit and never named by `output_digests`.
    pub async fn missing(&self, manifest: &TaskManifest) -> Option<Digest> {
        let writer = self.writer.lock().await;
        manifest
            .output_digests()
            .find(|digest| !writer.holds(digest))
    }

    /// The files of the output directories `manifest` names ([`crate::files`]): each Tree
    /// is the one [`Spool::keep_tree`] kept (the object's file is not read back), and each
    /// file's offset is where the object has it.
    ///
    /// # Errors
    /// A Tree that was not kept or is not a tree of records of the object, or the file refused
    /// the flush.
    pub async fn files(&self, manifest: &TaskManifest) -> io::Result<Vec<u8>> {
        let manifest = manifest.clone();
        let (writer, kept) = (Arc::clone(&self.writer), Arc::clone(&self.trees));
        let trees: Vec<(String, Digest)> = (manifest.output_directories.iter())
            .map(|dir| (dir.path.clone(), dir.tree))
            .collect();
        let failure = Arc::clone(&self.failure);
        let done = tokio::task::spawn_blocking(move || {
            let mut writer = writer.blocking_lock();
            writer.sink().flush().map_err(|e| record(&failure, e))?;
            let mut dirs = Vec::new();
            for (path, tree) in trees {
                if writer.offset_of(&tree).is_none() {
                    return Err(io::Error::other(format!("the Tree {tree} is not a record")));
                }
                let kept = kept.lock().unwrap_or_else(PoisonError::into_inner);
                let bytes = kept
                    .get(&tree)
                    .ok_or_else(|| io::Error::other(format!("the Tree {tree} was not kept")))?;
                dirs.push((
                    path,
                    reapi::Tree::decode(bytes.as_slice()).map_err(io::Error::other)?,
                ));
            }
            files::build(&manifest, &dirs, &|digest| writer.offset_of(digest))
                .map_err(io::Error::other)
        })
        .await;
        done.map_err(io::Error::other)?
    }

    /// Ends the object with `manifest` (its JSON), the `files`, the index and the trailer, flushes it, and
    /// returns what was written. Every other holder of the spool must be gone.
    ///
    /// # Errors
    /// The file refused a write, or something still holds the spool.
    pub fn finish(self: Arc<Self>, manifest: &[u8], files: &[u8]) -> io::Result<Written> {
        let busy = || io::Error::other("the object is still being written");
        let this = Arc::try_unwrap(self).map_err(|_| busy())?;
        let writer = Arc::try_unwrap(this.writer)
            .map_err(|_| busy())?
            .into_inner();
        let record = |error| record(&this.failure, error);
        let (mut sink, written) = writer.finish(manifest, files).map_err(record)?;
        sink.flush().map_err(record)?;
        Ok(written)
    }
}

impl Tee for Spool {
    fn open(&self) -> io::Result<Box<dyn TeeFile>> {
        // Called on the blocking pool; waits for a blob being written by `put`.
        let mut guard = Arc::clone(&self.writer).blocking_lock_owned();
        let at = guard
            .begin()
            .map_err(|error| record(&self.failure, error))?;
        Ok(Box::new(Streamed {
            guard,
            at,
            failure: Arc::clone(&self.failure),
        }))
    }
}

/// One file being teed into the object: it holds the writer from its open to its end.
struct Streamed {
    guard: OwnedMutexGuard<Writer>,
    at: Streaming,
    failure: Remembered,
}

impl TeeFile for Streamed {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        (self.guard.chunk(&mut self.at, bytes)).map_err(|error| record(&self.failure, error))
    }

    fn end(self: Box<Self>, digest: &reapi::Digest) -> io::Result<()> {
        let digest = digest::from_reapi(digest).map_err(io::Error::other)?;
        let Self { mut guard, at, .. } = *self;
        guard.end(at, &digest).map(drop)
    }
}

#[cfg(test)]
#[path = "spool_tests.rs"]
mod tests;
