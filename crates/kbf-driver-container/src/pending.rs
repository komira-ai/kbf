//! [`Pending`]: the blobs a walk of an action's outputs found, stored once the CAS has
//! said which of them it lacks.
//!
//! The walk hashes every output file and closes it, and builds every Directory and the
//! Tree; [`Pending::store_missing`] then asks the CAS about all of their digests in
//! one [`Cas::missing`] (the transfer engine cuts it into FindMissing calls of
//! `--transfer-find-missing-chunk` and sends them at once). When each upload asked for
//! itself, the walk's bound on open files (64 uploads at a time) was also the bound on
//! the questions in a call, and the calls went one after another: 14,245 outputs the
//! server mostly held took 785 s to upload.
//!
//! Only the blobs the CAS lacks are copied, [`Cas::in_flight`] at a time, into a
//! [`Spool`] (one unlinked local file), and the spool is stored with
//! [`Cas::put_spool`]: the transfer engine sends it as whole segments with one commit
//! per upload (`docs/design/write-path.md`, section 4), as the native driver's
//! outputs go, so a tree of thousands of new files is a few calls, not one ByteStream
//! write per file. A server without segment upload gets them one write each.
//!
//! A missing file is opened again to be copied, by the names the walk took from the
//! upper directory, each directory relative to its parent's descriptor with
//! `O_NOFOLLOW`, the file too ([`FileAt::open`]). It must be the file the walk hashed
//! (device and inode): one replaced or removed since is an error. Bytes changed in
//! place no longer hash to the digest, and the spool refuses them. Only the copies in
//! flight hold a descriptor, never the whole tree, and none is open while the spool
//! is uploaded.

use std::collections::HashSet;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::{StreamExt as _, TryStreamExt as _, future, stream};
use kbf_daemon::cas::{Cas, Spool, digest_of, label};
use kbf_proto::reapi::Digest;
use rustix::fs::Mode;

use crate::cas::FileBlob;
use crate::outputs::{FILE, Identity, blocking, descend, identity};
use crate::tree::TreeError;

/// The distinct blobs of one action's outputs, in the order the walk found them.
#[derive(Default)]
pub(crate) struct Pending {
    blobs: Vec<(Digest, Blob)>,
    found: HashSet<String>,
}

/// Where a pending blob's bytes are.
enum Blob {
    /// A Directory or Tree message.
    Bytes(Vec<u8>),
    /// An output file, hashed and closed.
    File(FileAt),
}

/// A file the walk hashed: where it is below the upper directory, and which file it
/// was.
pub(crate) struct FileAt {
    /// The directories from the upper directory down to the file's, by name.
    pub dirs: Arc<[String]>,
    pub name: String,
    /// The file's device and inode when it was hashed.
    pub id: Identity,
    /// The path shown in errors; never opened.
    pub shown: PathBuf,
}

impl FileAt {
    /// Opens the file again below `upper`, never following a symlink, and checks it is
    /// still the file that was hashed.
    pub(crate) fn open(&self, upper: &OwnedFd) -> std::io::Result<std::fs::File> {
        let replaced = || std::io::Error::other("was removed or replaced since it was hashed");
        let dir = descend(upper, &self.dirs)?.ok_or_else(replaced)?;
        let fd = rustix::fs::openat(&dir, self.name.as_str(), FILE, Mode::empty())?;
        if identity(&fd)? != self.id {
            return Err(replaced());
        }
        Ok(std::fs::File::from(fd))
    }
}

impl Pending {
    /// Adds `bytes` (a Directory or Tree message), unless a blob with its digest was
    /// added already, and returns the digest.
    pub(crate) fn bytes(&mut self, bytes: Vec<u8>) -> Digest {
        let digest = digest_of(&bytes);
        self.add(&digest, || Blob::Bytes(bytes));
        digest
    }

    /// Adds the file `at`, hashed to `digest`, unless a blob with that digest was added
    /// already.
    pub(crate) fn file(&mut self, digest: &Digest, at: FileAt) {
        self.add(digest, || Blob::File(at));
    }

    fn add(&mut self, digest: &Digest, blob: impl FnOnce() -> Blob) {
        if self.found.insert(label(digest)) {
            self.blobs.push((digest.clone(), blob()));
        }
    }

    /// Asks `cas` which blobs it lacks, all at once, copies those into a [`Spool`] made
    /// in `spools`, and stores the spool with [`Cas::put_spool`]: the transfer engine
    /// sends it as a few whole segments with one commit each, or blob by blob to a
    /// server without segment upload. Files are opened again below `upper`, at most
    /// [`Cas::in_flight`] at a time ([`Pending::copy_missing`]). Fails with the first
    /// blob that cannot be copied or stored.
    pub(crate) async fn store_missing(
        self,
        cas: &impl Cas,
        upper: &Arc<OwnedFd>,
        spools: &Path,
    ) -> Result<(), TreeError> {
        let digests: Vec<Digest> = self.blobs.iter().map(|(d, _)| d.clone()).collect();
        let missing: HashSet<String> = cas.missing(&digests).await.iter().map(label).collect();
        let dir = spools.to_owned();
        let spool = Arc::new(blocking(spools, move || Spool::create(&dir)).await?);
        self.copy_missing(&missing, &*spool, upper, cas.in_flight())
            .await?;
        Ok(cas.put_spool(spool).await?)
    }

    /// Stores the blobs `missing` names (labels) into `into`, at most `in_flight` at a
    /// time, so at most that many files are open; files are opened again below
    /// `upper`. Fails with the first blob that cannot be stored.
    async fn copy_missing(
        self,
        missing: &HashSet<String>,
        into: &impl Cas,
        upper: &Arc<OwnedFd>,
        in_flight: usize,
    ) -> Result<(), TreeError> {
        stream::iter(self.blobs)
            .filter(|(digest, _)| future::ready(missing.contains(&label(digest))))
            .map(|(digest, blob)| store(into, upper, digest, blob))
            .buffer_unordered(in_flight.max(1))
            .try_for_each(|()| future::ready(Ok(())))
            .await
    }
}

/// Stores one pending blob.
async fn store(
    cas: &impl Cas,
    upper: &Arc<OwnedFd>,
    digest: Digest,
    blob: Blob,
) -> Result<(), TreeError> {
    match blob {
        Blob::Bytes(bytes) => cas.put(bytes).await?,
        Blob::File(at) => {
            let upper = Arc::clone(upper);
            let shown = at.shown.clone();
            let file = blocking(&shown, move || at.open(&upper)).await?;
            FileBlob::hashed_as(file, digest).store(cas).await?
        }
    };
    Ok(())
}

#[cfg(test)]
#[path = "pending_tests.rs"]
mod tests;
