//! Unpacking a component's tarball: exactly one regular file, with the component's name.
//!
//! A release artifact is `kbf-<commit>-<platform>.tar.gz`, which holds one file named for
//! the binary (`kbf` or `kbf-daemon`), with no directory in front of it. This is only
//! ever done to a tarball the attestation already accepted, and still refuses anything
//! else: a second entry, a path (`./kbf`, `../kbf`, `bin/kbf`), a link, a directory, a
//! device, a sparse file or an entry larger than the cap. Nothing is written until the
//! first entry's name, type and size have been checked, and at most the cap's bytes are.

use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Instant;

use flate2::read::GzDecoder;
use tar::{Archive, EntryType};

use crate::refusal::ArchiveFault;

/// Write the one file `tarball` holds, which must be named `name`, to `dest` (a new file,
/// mode 0755).
pub fn unpack_one(tarball: &Path, name: &str, dest: &Path, cap: u64) -> Result<(), ArchiveFault> {
    unpack_one_until(tarball, name, dest, cap, None)
}

/// A reader that fails once `deadline` has passed: a read of the tarball is the only place
/// an unpack waits, so the wall-clock bound of an extraction is a check at each one.
struct Until<R> {
    inner: R,
    deadline: Option<Instant>,
}

impl<R: Read> Read for Until<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the extraction ran out of time",
            ));
        }
        self.inner.read(buf)
    }
}

/// [`unpack_one`], failing (as a malformed archive, naming the time) when `deadline` passes.
pub fn unpack_one_until(
    tarball: &Path,
    name: &str,
    dest: &Path,
    cap: u64,
    deadline: Option<Instant>,
) -> Result<(), ArchiveFault> {
    let bad = |e: io::Error| ArchiveFault::NotTarGz(e.to_string());
    let file = Until {
        inner: File::open(tarball).map_err(bad)?,
        deadline,
    };
    let mut archive = Archive::new(GzDecoder::new(file));
    let mut seen = 0usize;
    for entry in archive.entries().map_err(bad)? {
        let mut entry = entry.map_err(bad)?;
        seen += 1;
        if seen > 1 {
            // A second entry: the count is all that is read of it.
            continue;
        }
        if entry.path_bytes().as_ref() != name.as_bytes() {
            return Err(ArchiveFault::WrongName(name.to_owned()));
        }
        if entry.header().entry_type() != EntryType::Regular {
            return Err(ArchiveFault::NotRegular);
        }
        if entry.size() > cap {
            return Err(ArchiveFault::Oversize { cap });
        }
        let mut out = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .open(dest)
            .map_err(bad)?;
        // The reader gives the header's size and no more, and the header was checked.
        io::copy(&mut (&mut entry).take(cap), &mut out).map_err(bad)?;
        out.sync_all().map_err(bad)?;
    }
    match seen {
        1 => Ok(()),
        n => Err(ArchiveFault::Entries(n)),
    }
}

#[cfg(test)]
#[path = "archive_tests.rs"]
mod tests;
