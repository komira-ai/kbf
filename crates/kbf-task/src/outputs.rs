//! Collecting an action's declared outputs into the run's one object.
//!
//! [`collect_spooled`] reads the outputs the way the container driver does
//! ([`kbf_outputs::collect`]: files, directories as Trees, symlinks, the executable bit, nothing
//! followed through a symlink, within [`OutputLimits`]) and hashes the stdout and stderr files.
//! The [`Store`] it hands `kbf-outputs` is a [`Spooler`]: where the driver's store uploads, this
//! one writes each distinct blob to the [`Spool`] of the object, a Directory or Tree blob and a
//! file of at most one chunk from memory, a larger file as it is hashed, so every output is read
//! once and none is held whole.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use kbf_outputs::{OutputsError, Store, StoreError, Tee, digest_of, store_file};
use kbf_proto::reapi::{ActionResult, Digest};

pub use kbf_outputs::OutputLimits;

use crate::spool::Spool;

/// How many blobs the store accepts at once; it does no I/O of its own, so any small number
/// serves.
const IN_FLIGHT: usize = 4;

/// A [`Store`] that writes what it is given to the object, and uploads nothing.
struct Spooler {
    spool: Arc<Spool>,
}

impl Store for Spooler {
    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, StoreError> {
        let digest = digest_of(&bytes);
        self.spool.put(&digest, bytes).await?;
        Ok(digest)
    }

    async fn put_file(&self, _file: File, _digest: Digest) -> Result<Digest, StoreError> {
        Err("a file was handed over although its bytes go to the object".into())
    }

    fn keep_tree(&self, bytes: &[u8]) {
        self.spool.keep_tree(bytes);
    }

    fn in_flight(&self) -> usize {
        IN_FLIGHT
    }

    fn tee(&self) -> Option<Arc<dyn Tee>> {
        Some(Arc::clone(&self.spool) as Arc<dyn Tee>)
    }
}

/// Collects the outputs `paths` (relative to `working_directory`) under `root`, and the files
/// `stdout` and `stderr`, within `limits`, writing every blob to `spool` as it is read. The
/// returned result names the digests; the exit code and the timings are the caller's to set.
///
/// # Errors
/// An [`OutputsError`]: a bad output path, a limit passed, an unreadable output or log; one that
/// is an `Io` or `Store` error may be the spool's ([`Spool::write_failure`] says).
pub async fn collect_spooled(
    root: &Path,
    working_directory: &str,
    paths: &[String],
    stdout: &Path,
    stderr: &Path,
    limits: OutputLimits,
    spool: &Arc<Spool>,
) -> Result<ActionResult, OutputsError> {
    let store = Spooler {
        spool: Arc::clone(spool),
    };
    let mut result = ActionResult::default();
    kbf_outputs::collect(&store, root, working_directory, paths, limits, &mut result).await?;
    result.stdout_digest = Some(store_file(&store, stdout, limits.max_stdio_bytes).await?);
    result.stderr_digest = Some(store_file(&store, stderr, limits.max_stdio_bytes).await?);
    Ok(result)
}

#[cfg(test)]
#[path = "outputs_tests.rs"]
mod tests;
