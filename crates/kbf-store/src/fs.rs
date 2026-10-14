//! The filesystem the store writes through: one directory of flat files.
//!
//! The store never touches the disk except through [`Fs`] and [`FsFile`], so a test can
//! swap in [`FaultFs`](crate::FaultFs), which fails or crashes at any chosen operation
//! and then shows what a power cut would have left on disk. [`StdFs`] is the real one.
//!
//! A file is opened in one of two modes. [`Fs::create`] and [`Fs::open_append`] open it
//! for [`FsFile::append`] (the store's log and state files); [`Fs::create_positioned`]
//! opens it for [`FsFile::write_at`] (the write tier's block files, written by several
//! writers into disjoint ranges). A call of the other mode fails with
//! [`io::ErrorKind::InvalidInput`]: on Linux a positioned write to a file opened for
//! appending appends instead, at the end, whatever the offset.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

/// One directory the store owns. Names are plain file names, never paths.
///
/// What is durable follows POSIX: bytes appended to a file are durable once
/// [`FsFile::sync`] on that same handle returns `Ok`, and a created, renamed or removed
/// name is durable once [`Fs::sync_dir`] returns `Ok`.
pub trait Fs {
    /// An open file.
    type File: FsFile;

    /// The names in the directory, in any order.
    ///
    /// # Errors
    ///
    /// The I/O error that stopped the listing.
    fn list(&self) -> io::Result<Vec<String>>;

    /// The whole content of `name`.
    ///
    /// # Errors
    ///
    /// The I/O error that stopped the read.
    fn read(&self, name: &str) -> io::Result<Vec<u8>>;

    /// The length of `name` in bytes.
    ///
    /// # Errors
    ///
    /// The I/O error that stopped the lookup, including `name` missing.
    fn file_len(&self, name: &str) -> io::Result<u64>;

    /// `len` bytes of `name` from `offset`.
    ///
    /// # Errors
    ///
    /// The I/O error that stopped the read; [`io::ErrorKind::UnexpectedEof`] if the
    /// file ends before `offset + len`.
    fn read_at(&self, name: &str, offset: u64, len: usize) -> io::Result<Vec<u8>>;

    /// Creates `name`, which must not exist, and opens it for appending.
    ///
    /// # Errors
    ///
    /// The I/O error that stopped the create, including `name` existing.
    fn create(&self, name: &str) -> io::Result<Self::File>;

    /// Creates `name`, which must not exist, and opens it for positioned writes
    /// ([`FsFile::write_at`]); [`FsFile::append`] on it fails.
    ///
    /// # Errors
    ///
    /// The I/O error that stopped the create, including `name` existing.
    fn create_positioned(&self, name: &str) -> io::Result<Self::File>;

    /// Opens the existing `name` for appending.
    ///
    /// # Errors
    ///
    /// The I/O error that stopped the open.
    fn open_append(&self, name: &str) -> io::Result<Self::File>;

    /// Renames `from` to `to`, replacing `to` if it exists.
    ///
    /// # Errors
    ///
    /// The I/O error that stopped the rename.
    fn rename(&self, from: &str, to: &str) -> io::Result<()>;

    /// Removes `name`.
    ///
    /// # Errors
    ///
    /// The I/O error that stopped the removal.
    fn remove(&self, name: &str) -> io::Result<()>;

    /// Makes every create, rename and removal so far durable.
    ///
    /// # Errors
    ///
    /// The I/O error the directory sync returned.
    fn sync_dir(&self) -> io::Result<()>;
}

/// A file opened for appending or for positioned writes (see the module docs).
pub trait FsFile {
    /// Appends all of `bytes`.
    ///
    /// # Errors
    ///
    /// The I/O error that stopped the write; [`io::ErrorKind::InvalidInput`] on a file
    /// opened for positioned writes.
    fn append(&mut self, bytes: &[u8]) -> io::Result<()>;

    /// Writes all of `bytes` at `offset`, extending the file (with zeros over any gap)
    /// if it ends before them. It takes `&self`: positioned writes share no cursor, so
    /// several writers may write disjoint ranges of one file at once.
    ///
    /// # Errors
    ///
    /// The I/O error that stopped the write; [`io::ErrorKind::InvalidInput`] on a file
    /// opened for appending.
    fn write_at(&self, offset: u64, bytes: &[u8]) -> io::Result<()>;

    /// Cuts the file to `len` bytes.
    ///
    /// # Errors
    ///
    /// The I/O error that stopped the truncation.
    fn truncate(&mut self, len: u64) -> io::Result<()>;

    /// Makes the file's content and length durable.
    ///
    /// # Errors
    ///
    /// The I/O error the sync returned. After one, what reached the disk is unknown.
    fn sync(&self) -> io::Result<()>;
}

/// The real filesystem: one directory on the local disk.
#[derive(Debug)]
pub struct StdFs {
    dir: PathBuf,
}

impl StdFs {
    /// The directory `dir`, created (with its parents) if missing.
    ///
    /// # Errors
    ///
    /// The I/O error that stopped creating the directory.
    pub fn new(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// The directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Fs for StdFs {
    type File = StdFile;

    fn list(&self) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let name = entry?.file_name();
            names.push(name.into_string().map_err(|n| {
                io::Error::new(io::ErrorKind::InvalidData, format!("file name {n:?}"))
            })?);
        }
        Ok(names)
    }

    fn read(&self, name: &str) -> io::Result<Vec<u8>> {
        std::fs::read(self.path(name))
    }

    fn file_len(&self, name: &str) -> io::Result<u64> {
        Ok(std::fs::metadata(self.path(name))?.len())
    }

    fn read_at(&self, name: &str, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut buf = vec![0; len];
        File::open(self.path(name))?.read_exact_at(&mut buf, offset)?;
        Ok(buf)
    }

    fn create(&self, name: &str) -> io::Result<StdFile> {
        let file = OpenOptions::new()
            .append(true)
            .create_new(true)
            .open(self.path(name))?;
        Ok(StdFile::new(file, Mode::Append))
    }

    fn create_positioned(&self, name: &str) -> io::Result<StdFile> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.path(name))?;
        Ok(StdFile::new(file, Mode::Positioned))
    }

    fn open_append(&self, name: &str) -> io::Result<StdFile> {
        let file = OpenOptions::new().append(true).open(self.path(name))?;
        Ok(StdFile::new(file, Mode::Append))
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        std::fs::rename(self.path(from), self.path(to))
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        std::fs::remove_file(self.path(name))
    }

    fn sync_dir(&self) -> io::Result<()> {
        File::open(&self.dir)?.sync_all()
    }
}

/// How a file was opened: which of [`FsFile::append`] and [`FsFile::write_at`] it
/// takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Append,
    Positioned,
}

impl Mode {
    /// Fails unless the file was opened in mode `want`.
    pub(crate) fn require(self, want: Self) -> io::Result<()> {
        if self == want {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("a file opened for {self:?} writes takes no {want:?} write"),
            ))
        }
    }
}

/// A file of [`StdFs`]. Every sync goes through the handle that wrote, so a write-back
/// error on it is reported to this handle.
#[derive(Debug)]
pub struct StdFile {
    file: File,
    mode: Mode,
}

impl StdFile {
    const fn new(file: File, mode: Mode) -> Self {
        Self { file, mode }
    }
}

impl FsFile for StdFile {
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.mode.require(Mode::Append)?;
        self.file.write_all(bytes)
    }

    fn write_at(&self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        self.mode.require(Mode::Positioned)?;
        self.file.write_all_at(bytes, offset)
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }

    fn sync(&self) -> io::Result<()> {
        self.file.sync_data()
    }
}
