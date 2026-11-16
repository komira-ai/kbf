//! The put directory (`--stage-and-commit`): the run's one object is written as a file into
//! `--out-dir` and committed by `rename`, instead of PUT to presigned URLs.
//!
//! In a job the directory is a mount of tierstore's put directory: a file written there and
//! renamed to a name without `.part` becomes an object under the key prefix the frontend chose,
//! and the `rename` returns only once the store has it. kbf-task does not know the key or the
//! store; it knows the name, `pack`.
//!
//! [`OutDir::object`] creates `pack.part`, new (`O_EXCL`), and returns the [`ObjectRun`] the
//! outputs are written through as they are collected, read once, in one pass (the object's format
//! is in [`crate::spool`], and in docs/design/one-pass-task-object.md). [`ObjectRun::finish`] ends
//! it with the manifest and commits it: the file is renamed to `pack.sha256-<hex>.part`, which
//! tells the store the digest to expect, and then committed by renaming that to `pack`.
//!
//! The run's exit code is 0 only when the commit `rename` returned 0 (the Command's own exit
//! code is in the manifest). A commit that fails with `EBUSY` (the store still sees the file
//! open), `EIO` (the store did not take it before its deadline) or `EINTR` is tried again, up to
//! [`COMMIT_ATTEMPTS`] times in all, after the pauses of [`PAUSES`]; any other answer, or the last
//! attempt's, fails the run (exit 13) with the errno and what the store means by it (`EEXIST`:
//! the key holds other bytes; `EBADMSG`: the bytes are not the digest's; `EFBIG`: too large for
//! the store). A file that cannot be created or written is exit 13 too. Once a run fails, its
//! `.part` file is removed.
//!
//! The file is not synced and its close is not checked: the commit `rename` is the only point
//! the store acts on, and it refuses bytes that are not the digest the name gives.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use std::sync::Arc;

use kbf_task_pack::TaskManifest;
use kbf_types::Digest;

use crate::pack::{MAX_LARGE_BYTES, PackError};
use crate::spool::Spool;
use crate::task::Failure;

/// The pauses before the second and later attempts of a commit `rename`.
pub(crate) const PAUSES: [Duration; 3] = [
    Duration::from_millis(100),
    Duration::from_secs(1),
    Duration::from_secs(5),
];

/// The most attempts of one commit `rename`.
pub(crate) const COMMIT_ATTEMPTS: usize = PAUSES.len() + 1;

/// The name of the run's one object.
pub(crate) const OBJECT_NAME: &str = "pack";

/// The name an object is written under: `<name>.part`.
#[must_use]
pub(crate) fn part_name(name: &str) -> String {
    format!("{name}.part")
}

/// The name that tells the store the digest to expect: `<name>.sha256-<hex>.part`.
#[must_use]
pub(crate) fn digest_part_name(name: &str, digest: &Digest) -> String {
    format!("{name}.sha256-{}.part", hex::encode(digest.hash))
}

/// Why an object was not written or committed. Each names the object.
#[derive(Debug, thiserror::Error)]
pub enum OutDirError {
    /// `<name>.part` could not be created.
    #[error("{name}: cannot create it in the put directory: {source}")]
    Create {
        /// The object's name.
        name: String,
        /// The error of the create.
        #[source]
        source: io::Error,
    },
    /// `<name>.part` could not be written.
    #[error("{name}: cannot write it to the put directory: {source}")]
    Write {
        /// The object's name.
        name: String,
        /// The error of the write.
        #[source]
        source: io::Error,
    },
    /// `<name>.part` could not be renamed to the name of its digest.
    #[error("{name}: cannot rename it to the name of its digest: {source}")]
    Digest {
        /// The object's name.
        name: String,
        /// The error of the rename.
        #[source]
        source: io::Error,
    },
    /// The commit `rename` to `<name>` failed.
    #[error(
        "{name}: the commit failed on attempt {attempts} of {COMMIT_ATTEMPTS}: {source}{}",
        meaning(source)
    )]
    Commit {
        /// The object's name.
        name: String,
        /// The attempt that failed last.
        attempts: usize,
        /// The error of that attempt.
        #[source]
        source: io::Error,
    },
}

/// What the store means by the errno of a failed commit, as the end of a message.
fn meaning(error: &io::Error) -> &'static str {
    match error.raw_os_error() {
        Some(libc::EEXIST) => "; the key holds another object",
        Some(libc::EBADMSG) => "; the bytes are not those of the digest the name gives",
        Some(libc::EFBIG) => "; the object is larger than the store takes",
        Some(libc::EIO) => "; the store did not take it before its deadline",
        Some(libc::EBUSY) => "; the store still sees the file open",
        _ => "",
    }
}

/// Whether a failed commit is tried again.
fn retried(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EBUSY | libc::EIO | libc::EINTR)
    )
}

/// Creates a file, new, for writing.
type Create = dyn Fn(&Path) -> io::Result<Box<dyn Write + Send>>;

/// Renames a file: the digest rename and the commit.
type Rename = dyn Fn(&Path, &Path) -> io::Result<()>;

/// The real create: `O_CREAT | O_EXCL`, so a name that is already there is refused.
fn create_new(path: &Path) -> io::Result<Box<dyn Write + Send>> {
    let file = OpenOptions::new().write(true).create_new(true).open(path)?;
    Ok(Box::new(file))
}

/// The real rename: the file system's, which in tierstore's put directory, for a name without
/// `.part`, returns once the store has the object.
fn rename(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::rename(from, to)
}

/// The put directory a run writes into.
pub struct OutDir {
    dir: PathBuf,
    create: Box<Create>,
    rename: Box<Rename>,
    pause: Box<dyn Fn(Duration)>,
}

impl OutDir {
    /// The put directory at `dir`, which must be a directory (a symlink to one is followed).
    /// Files are created and renamed through the file system, and the pauses between attempts
    /// of a commit are sleeps.
    ///
    /// # Errors
    /// [`Failure::Setup`] when `dir` is not a directory or cannot be looked at.
    pub fn open(dir: &Path) -> Result<Self, Failure> {
        let setup = |why: String| Failure::Setup(format!("--out-dir {}: {why}", dir.display()));
        match std::fs::metadata(dir) {
            Ok(meta) if meta.is_dir() => Ok(Self {
                dir: dir.to_owned(),
                create: Box::new(create_new),
                rename: Box::new(rename),
                pause: Box::new(std::thread::sleep),
            }),
            Ok(_) => Err(setup("not a directory".to_owned())),
            Err(e) => Err(setup(e.to_string())),
        }
    }

    /// The same directory, creating its files through `create`.
    #[cfg(test)]
    pub(crate) fn with_create(
        self,
        create: impl Fn(&Path) -> io::Result<Box<dyn Write + Send>> + 'static,
    ) -> Self {
        Self {
            create: Box::new(create),
            ..self
        }
    }

    /// The same directory, renaming (the digest rename and the commit) through `rename`.
    #[cfg(test)]
    pub(crate) fn with_rename(
        self,
        rename: impl Fn(&Path, &Path) -> io::Result<()> + 'static,
    ) -> Self {
        Self {
            rename: Box::new(rename),
            ..self
        }
    }

    /// The same directory, pausing between attempts through `pause` instead of a sleep.
    #[cfg(test)]
    pub(crate) fn with_pause(self, pause: impl Fn(Duration) + 'static) -> Self {
        Self {
            pause: Box::new(pause),
            ..self
        }
    }

    /// The directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Starts the one object of a one-pass run (`--object-format one-pass`): creates `pack.part`,
    /// new, and returns the run to write it through. Nothing is committed until
    /// [`ObjectRun::finish`]; a run dropped before it removes the file.
    ///
    /// # Errors
    /// [`OutDirError::Create`] (exit 13).
    pub fn object(&self) -> Result<ObjectRun<'_>, Failure> {
        let mut stage = Stage::new(self);
        let name = OBJECT_NAME;
        let part = self.dir.join(part_name(name));
        let file = (self.create)(&part).map_err(|source| OutDirError::Create {
            name: name.to_owned(),
            source,
        })?;
        let read = part.clone();
        stage.parts.push(part);
        Ok(ObjectRun {
            stage,
            spool: Spool::new(
                file,
                Box::new(move |offset, len| read_at(&read, offset, len)),
            ),
        })
    }
}

/// The one object of a one-pass run while it is written.
pub struct ObjectRun<'a> {
    stage: Stage<'a>,
    spool: Arc<Spool>,
}

impl ObjectRun<'_> {
    /// Where the outputs are written as they are collected.
    #[must_use]
    pub fn spool(&self) -> &Arc<Spool> {
        &self.spool
    }

    /// Ends the object with `manifest` (`manifest.timing.pack_ms` is, on entry, the time the
    /// outputs took to collect, which is the whole of the walk, hash and write: they are one
    /// pass), and commits it: the digest rename, then the commit rename.
    ///
    /// # Errors
    /// A [`Failure`]: a manifest the shared rules refuse, or one that names a blob the object
    /// does not hold (exit 12); the object's file refusing a write, or a commit that fails
    /// (exit 13). The `.part` file is removed.
    pub async fn finish(self, manifest: &mut TaskManifest) -> Result<(), Failure> {
        let Self { mut stage, spool } = self;
        let name = OBJECT_NAME;
        if let Some(digest) = spool.missing(manifest).await {
            return Err(PackError::missing(&digest).into());
        }
        manifest.timing.walk_ms = Some(manifest.timing.pack_ms);
        let files = spool.files(manifest).await.map_err(write_error(name))?;
        let json = manifest
            .to_json_with(MAX_LARGE_BYTES)
            .map_err(PackError::from)?;
        let ended = tokio::task::spawn_blocking(move || spool.finish(&json, &files)).await;
        let written = ended
            .map_err(io::Error::other)
            .and_then(|done| done)
            .map_err(write_error(name))?;
        stage.commit(name, &written.object)
    }
}

/// `len` bytes at `offset` of the file at `path`.
fn read_at(path: &Path, offset: u64, len: u64) -> io::Result<Vec<u8>> {
    use std::os::unix::fs::FileExt as _;
    let mut bytes = vec![0; usize::try_from(len).map_err(io::Error::other)?];
    std::fs::File::open(path)?.read_exact_at(&mut bytes, offset)?;
    Ok(bytes)
}

/// The `.part` files of one run that are not committed yet; they are removed when it is
/// dropped, so a failed run leaves none.
struct Stage<'a> {
    out: &'a OutDir,
    parts: Vec<PathBuf>,
}

impl<'a> Stage<'a> {
    fn new(out: &'a OutDir) -> Self {
        Self {
            out,
            parts: Vec::new(),
        }
    }

    /// Renames `<name>.part` to the name of `digest`, then commits it as `name`, trying a commit
    /// that failed with `EBUSY`, `EIO` or `EINTR` again after each of [`PAUSES`].
    fn commit(&mut self, name: &str, digest: &Digest) -> Result<(), Failure> {
        let part = self.out.dir.join(part_name(name));
        let staged = self.out.dir.join(digest_part_name(name, digest));
        (self.out.rename)(&part, &staged).map_err(|source| OutDirError::Digest {
            name: name.to_owned(),
            source,
        })?;
        self.parts.retain(|p| *p != part);
        self.parts.push(staged.clone());
        let to = self.out.dir.join(name);
        for attempt in 1.. {
            match (self.out.rename)(&staged, &to) {
                Ok(()) => break,
                Err(source) if retried(&source) && attempt < COMMIT_ATTEMPTS => {
                    (self.out.pause)(PAUSES[attempt - 1]);
                }
                Err(source) => {
                    return Err(OutDirError::Commit {
                        name: name.to_owned(),
                        attempts: attempt,
                        source,
                    }
                    .into());
                }
            }
        }
        self.parts.retain(|p| *p != staged);
        Ok(())
    }
}

impl Drop for Stage<'_> {
    fn drop(&mut self) {
        for part in &self.parts {
            // Best effort: a file that cannot be removed is left to the store's sweep.
            let _ = std::fs::remove_file(part);
        }
    }
}

/// The error of a write to `name`.
fn write_error(name: &str) -> impl Fn(io::Error) -> OutDirError + '_ {
    move |source| OutDirError::Write {
        name: name.to_owned(),
        source,
    }
}

#[cfg(test)]
#[path = "out_dir_tests.rs"]
mod tests;
