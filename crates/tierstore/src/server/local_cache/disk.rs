//! The calls the local cache makes on the file system that a test must be able to replace: the
//! free space (so that a test sets how full the disk is), the create-only link, the link count
//! and the time of a file, and the write of a chunk of a put to its stage file. Everything else
//! the cache does with `std::fs` on a directory of its own.

use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use futures::FutureExt as _;
use futures::future::BoxFuture;
use rustix::fs::{AtFlags, CWD, Timespec, Timestamps, statvfs, utimensat};

/// The size of a file system and what is left of it for a process that is not root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Space {
    /// The bytes the file system holds.
    pub total: u64,
    /// The bytes a process that is not root may still write.
    pub free: u64,
}

/// The file system under the cache.
pub trait Disk: Send + Sync {
    /// The size and the free space of the file system that holds `path` (`statvfs`).
    ///
    /// # Errors
    /// What the system said.
    fn space(&self, path: &Path) -> io::Result<Space>;

    /// Gives the file `from` a second name `to`, only if `to` does not exist (`link`): an
    /// existing `to` is [`io::ErrorKind::AlreadyExists`] and is left as it was.
    ///
    /// # Errors
    /// What the system said.
    fn link(&self, from: &Path, to: &Path) -> io::Result<()>;

    /// The number of names the file has (`st_nlink`).
    ///
    /// # Errors
    /// What the system said, such as that there is no such file.
    fn links(&self, path: &Path) -> io::Result<u64>;

    /// Sets the time of last access and of last change of the file to `millis` since the Unix
    /// epoch (`utimensat`).
    ///
    /// # Errors
    /// What the system said.
    fn touch(&self, path: &Path, millis: u64) -> io::Result<()>;

    /// Appends `bytes` to the stage file `file`, which the copy of a put is written to. The
    /// default writes it; it is a call of the disk so that a test can make a write wait or fail.
    fn write_stage<'a>(
        &'a self,
        file: &'a mut tokio::fs::File,
        bytes: &'a [u8],
    ) -> BoxFuture<'a, io::Result<()>> {
        async move { tokio::io::AsyncWriteExt::write_all(file, bytes).await }.boxed()
    }
}

/// The file system of the machine.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemDisk;

impl Disk for SystemDisk {
    fn space(&self, path: &Path) -> io::Result<Space> {
        let stat = statvfs(path)?;
        let unit = stat.f_frsize;
        Ok(Space {
            total: stat.f_blocks.saturating_mul(unit),
            free: stat.f_bavail.saturating_mul(unit),
        })
    }

    fn link(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::hard_link(from, to)
    }

    fn links(&self, path: &Path) -> io::Result<u64> {
        Ok(std::fs::metadata(path)?.nlink())
    }

    fn touch(&self, path: &Path, millis: u64) -> io::Result<()> {
        let seconds = i64::try_from(millis / 1000).unwrap_or(i64::MAX);
        let stamp = Timespec {
            tv_sec: seconds,
            tv_nsec: i64::try_from((millis % 1000) * 1_000_000).unwrap_or(0),
        };
        let both = Timestamps {
            last_access: stamp,
            last_modification: stamp,
        };
        utimensat(CWD, path, &both, AtFlags::empty())?;
        Ok(())
    }
}
