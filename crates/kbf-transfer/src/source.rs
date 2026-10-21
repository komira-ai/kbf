//! [`Record`]: one blob on its way out, and the [`Source`] its bytes are read from when
//! the upload needs them.
//!
//! A driver that has walked an action's outputs hashes every file once, and hands the
//! engine the digests with the places the bytes are ([`Cas::put_records`]). The engine
//! reads them straight from there, a range at a time, as it sends them: nothing is
//! copied first, and nothing is hashed again here (the server checks every record
//! against its digest as the bytes arrive; that check is what a file changed after the
//! walk fails).
//!
//! A [`FileSource`] still guards against that change itself, cheaply, before the server
//! has to: it opens the file by name below the directory the walk held, never following
//! a symlink, and refuses it unless it is the file the walk hashed ([`Stamp`]: device,
//! inode, size, modification and change time), at the start of every read and again
//! after it.
//!
//! [`Cas::put_records`]: crate::Cas::put_records

use std::fs::File;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::FileExt as _;
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;

/// Opens a directory, refusing a symlink in its place.
pub const OPEN_DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// Opens a regular file for reading, refusing a symlink, and never blocking on a FIFO
/// or claiming a terminal that replaced it after the `stat`.
pub const OPEN_FILE: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::NOCTTY)
    .union(OFlags::CLOEXEC);

/// Opens each of `parts` below `start` as a directory, never following a symlink.
/// `None` when one is absent, a file or a symlink.
///
/// # Errors
/// Any other failure of `openat`.
pub fn descend(start: &OwnedFd, parts: &[String]) -> io::Result<Option<OwnedFd>> {
    let mut here = start.as_fd().try_clone_to_owned()?;
    for part in parts {
        here = match rustix::fs::openat(&here, part.as_str(), OPEN_DIRECTORY, Mode::empty()) {
            Ok(fd) => fd,
            // ELOOP: a symlink stands there; ENOTDIR: a file (or, on some kernels, a
            // symlink under O_DIRECTORY).
            Err(Errno::NOENT | Errno::NOTDIR | Errno::LOOP) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
    }
    Ok(Some(here))
}

/// What identifies a file's content as the walk saw it: the file itself (device and
/// inode), its size, and when it was last written and last changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    /// Seconds and nanoseconds of the last write.
    pub mtime: (i64, i64),
    /// Seconds and nanoseconds of the last change of content or metadata.
    pub ctime: (i64, i64),
}

impl Stamp {
    /// The stamp of the open file `fd`.
    ///
    /// # Errors
    /// `fstat` failed.
    #[allow(clippy::unnecessary_cast)] // the field types differ between Linux and macOS
    pub fn of(fd: &impl AsFd) -> io::Result<Self> {
        let stat = rustix::fs::fstat(fd)?;
        Ok(Self {
            dev: stat.st_dev as u64,
            ino: stat.st_ino as u64,
            size: u64::try_from(stat.st_size).unwrap_or(0),
            mtime: (stat.st_mtime as i64, stat.st_mtime_nsec as i64),
            ctime: (stat.st_ctime as i64, stat.st_ctime_nsec as i64),
        })
    }
}

/// A regular file the walk hashed and closed, found again by name when it is sent.
#[derive(Clone, Debug)]
pub struct FileSource {
    /// The directory the names below are relative to: held open for the whole upload.
    root: Arc<OwnedFd>,
    /// The directories from `root` down to the file's, by name. Files of one directory
    /// share the allocation.
    dirs: Arc<[String]>,
    name: String,
    stamp: Stamp,
}

impl FileSource {
    /// The file `name` in `dirs` below `root`, which the walk hashed and saw as `stamp`.
    #[must_use]
    pub fn new(root: Arc<OwnedFd>, dirs: Arc<[String]>, name: String, stamp: Stamp) -> Self {
        Self {
            root,
            dirs,
            name,
            stamp,
        }
    }

    /// The file's path below the root, for messages; never opened.
    fn shown(&self) -> PathBuf {
        let mut path: PathBuf = self.dirs.iter().collect();
        path.push(&self.name);
        path
    }

    /// Opens the file again, never following a symlink, and checks it is still the file
    /// the walk hashed, unchanged.
    fn open(&self) -> io::Result<File> {
        let gone = || self.changed("was removed or replaced since it was hashed");
        let dir = descend(&self.root, &self.dirs)?.ok_or_else(gone)?;
        // A name that is gone, or a symlink now (`O_NOFOLLOW`), or anything else that
        // stops the open: the file is not what was hashed.
        let open = rustix::fs::openat(&dir, self.name.as_str(), OPEN_FILE, Mode::empty());
        let fd = open.map_err(|e| self.changed(&format!("could not be opened again: {e}")))?;
        self.check(&fd)?;
        Ok(File::from(fd))
    }

    fn check(&self, fd: &impl AsFd) -> io::Result<()> {
        if Stamp::of(fd)? != self.stamp {
            return Err(self.changed("changed since it was hashed"));
        }
        Ok(())
    }

    fn changed(&self, what: &str) -> io::Error {
        io::Error::other(format!("{}: {what}", self.shown().display()))
    }

    /// Fills `buf` from `at` bytes into the file.
    fn read_at(&self, at: u64, buf: &mut [u8]) -> io::Result<()> {
        let file = self.open()?;
        file.read_exact_at(buf, at)?;
        // A write that landed while the bytes were read: caught here, not by the server.
        self.check(&file)
    }
}

/// Where a blob's bytes are.
#[derive(Clone, Debug)]
pub enum Source {
    /// In memory: a Directory or Tree message, or a file whose bytes a store took
    /// as the walk read them ([`crate::Intake`]).
    Bytes(Bytes),
    /// A file the walk hashed.
    File(FileSource),
    /// Written at `offset` of an unlinked spool file ([`crate::Spool`]).
    Spool { file: Arc<File>, offset: u64 },
}

impl Source {
    /// Fills `buf` with the blob's bytes from `at` bytes into the blob.
    ///
    /// Blocking.
    ///
    /// # Errors
    /// The bytes cannot be read, or the file is no longer the one that was hashed.
    pub(crate) fn read_at(&self, at: u64, buf: &mut [u8]) -> io::Result<()> {
        match self {
            Self::Bytes(bytes) => {
                let from = usize::try_from(at).unwrap_or(usize::MAX);
                let to = from.saturating_add(buf.len());
                buf.copy_from_slice(bytes.get(from..to).ok_or(io::ErrorKind::UnexpectedEof)?);
                Ok(())
            }
            Self::File(file) => file.read_at(at, buf),
            Self::Spool { file, offset } => file.read_exact_at(buf, offset + at),
        }
    }
}

/// One blob to store: its digest and CRC-32C, computed once by whoever read its bytes,
/// and where the bytes are.
#[derive(Clone, Debug)]
pub struct Record {
    pub digest: kbf_types::Digest,
    /// The CRC-32C of the blob's bytes: it goes in the footer of the segment the blob is
    /// sent in.
    pub crc32c: u32,
    pub source: Source,
}

impl Record {
    /// The record of `bytes` held in memory.
    #[must_use]
    pub fn bytes(bytes: Vec<u8>) -> Self {
        Self {
            digest: kbf_segments::sha256(&bytes),
            crc32c: kbf_segments::crc32c(&bytes),
            source: Source::Bytes(Bytes::from(bytes)),
        }
    }

    /// The blob's bytes from `at`, `len` of them, read on the blocking pool.
    ///
    /// # Errors
    /// The bytes cannot be read.
    pub(crate) async fn read(&self, at: u64, len: u64) -> io::Result<Vec<u8>> {
        let (source, len) = (
            self.source.clone(),
            usize::try_from(len).map_err(io::Error::other)?,
        );
        tokio::task::spawn_blocking(move || {
            let mut bytes = vec![0; len];
            source.read_at(at, &mut bytes).map(|()| bytes)
        })
        .await
        .map_err(io::Error::other)
        .and_then(std::convert::identity)
    }
}
