//! [`Pending`]: the blobs a walk of an action's outputs found, stored once the CAS has
//! said which of them it lacks.
//!
//! The walk hashes every output file and closes it, and builds every Directory and the
//! Tree; [`Pending::store_missing`] then asks the CAS about all of their digests in
//! one [`Cas::missing`] (the transfer engine cuts it into FindMissing calls of
//! `--transfer-find-missing-chunk` and sends them at once) and stores only the blobs
//! it names, [`Cas::in_flight`] at a time. When each upload asked for itself, the
//! walk's bound on open files (64 uploads at a time) was also the bound on the
//! questions in a call, and the calls went one after another: 14,245 outputs the
//! server mostly held took 785 s to upload.
//!
//! A missing file is opened again to be stored, by the names the walk took from the
//! upper directory, each directory relative to its parent's descriptor with
//! `O_NOFOLLOW`, the file too ([`FileAt::open`]). It must be the file the walk hashed
//! (device and inode): one replaced or removed since is an error. Bytes changed in
//! place no longer hash to the digest, and the CAS refuses them. Only the uploads in
//! flight hold a descriptor, never the whole tree.

use std::collections::HashSet;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::{StreamExt as _, TryStreamExt as _, future, stream};
use kbf_daemon::cas::{Cas, digest_of, label};
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

    /// Asks `cas` which blobs it lacks, all at once, and stores those, at most
    /// [`Cas::in_flight`] at a time; files are opened again below `upper`. Fails with
    /// the first blob that cannot be stored.
    pub(crate) async fn store_missing(
        self,
        cas: &impl Cas,
        upper: &Arc<OwnedFd>,
        _spools: &Path,
    ) -> Result<(), TreeError> {
        let digests: Vec<Digest> = self.blobs.iter().map(|(d, _)| d.clone()).collect();
        let missing: HashSet<String> = cas.missing(&digests).await.iter().map(label).collect();
        stream::iter(self.blobs)
            .filter(|(digest, _)| future::ready(missing.contains(&label(digest))))
            .map(|(digest, blob)| store(cas, upper, digest, blob))
            .buffer_unordered(cas.in_flight().max(1))
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
mod tests {
    use super::*;
    use crate::outputs::DIRECTORY;

    /// Catches a missing file opened again without checking it is the one hashed: a
    /// file replaced since (another inode at the name), a directory on its path
    /// replaced by a symlink, and a file removed must each fail, not be stored as the
    /// output. Mutants seen red: the device and inode check removed (the replaced file
    /// opens); a symlinked directory followed.
    #[test]
    fn a_file_is_opened_again_only_if_it_is_the_one_hashed() {
        let exe = std::env::current_exe().expect("test binary");
        let dir = exe
            .parent()
            .expect("deps directory")
            .join("kbf-driver-container-unit")
            .join("pending-open");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("upper/sub")).expect("mkdir");
        std::fs::create_dir_all(dir.join("elsewhere")).expect("mkdir");
        std::fs::write(dir.join("upper/sub/f"), b"hashed").expect("write");
        let upper =
            rustix::fs::openat(rustix::fs::CWD, dir.join("upper"), DIRECTORY, Mode::empty())
                .expect("open upper");
        let stat = rustix::fs::stat(dir.join("upper/sub/f")).expect("stat");
        let at = FileAt {
            dirs: vec!["sub".to_owned()].into(),
            name: "f".to_owned(),
            id: (stat.st_dev, stat.st_ino),
            shown: dir.join("upper/sub/f"),
        };
        let file = at.open(&upper).expect("the file hashed");
        assert_eq!(std::io::read_to_string(file).expect("read"), "hashed");
        // The directory replaced by a symlink to it: the file at the end is the one
        // hashed, but the path no longer is the action's.
        std::fs::rename(dir.join("upper/sub"), dir.join("elsewhere/sub")).expect("move");
        std::os::unix::fs::symlink(dir.join("elsewhere/sub"), dir.join("upper/sub"))
            .expect("symlink");
        let why = at.open(&upper).expect_err("a symlink on the path");
        assert!(why.to_string().contains("removed or replaced"), "{why}");
        std::fs::remove_file(dir.join("upper/sub")).expect("unlink");
        std::fs::rename(dir.join("elsewhere/sub"), dir.join("upper/sub")).expect("move back");
        at.open(&upper).expect("the file hashed, back in place");
        // Another file renamed over it: same name, same bytes, another inode.
        std::fs::write(dir.join("upper/sub/g"), b"hashed").expect("write");
        std::fs::rename(dir.join("upper/sub/g"), dir.join("upper/sub/f")).expect("rename");
        let why = at.open(&upper).expect_err("replaced");
        assert!(why.to_string().contains("removed or replaced"), "{why}");
        // The file removed from its directory.
        std::fs::remove_file(dir.join("upper/sub/f")).expect("unlink");
        let why = at.open(&upper).expect_err("removed");
        assert_eq!(why.kind(), std::io::ErrorKind::NotFound, "{why}");
    }
}
