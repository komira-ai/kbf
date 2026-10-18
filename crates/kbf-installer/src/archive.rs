//! Unpacking a component's tarball: exactly one regular file, with the component's name.
//!
//! A release artifact is `kbf-<commit>-<platform>.tar.gz`, which holds one file named for
//! the binary (`kbf` or `kbf-daemon`), with no directory in front of it. This is only
//! ever done to a tarball the attestation already accepted, and still refuses anything
//! else: a second entry, a path (`./kbf`, `../kbf`, `bin/kbf`), a link, a directory, a
//! device, a sparse file or an entry larger than the cap. Nothing is written until the
//! first entry's name, type and size have been checked, and at most the cap's bytes are.

use std::cell::Cell;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use flate2::bufread::GzDecoder;
use tar::{Archive, EntryType};

use crate::refusal::ArchiveFault;

/// What the tar framing around the one entry may add to the decompressed stream (the
/// header, the padding and the zero blocks at the end), beyond the entry's own cap.
const FRAMING: u64 = 64 * 1024;

/// Why the bounded reader stopped, when it was the reader that stopped the read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Trip {
    Size,
    Time,
}

/// The decompressed stream, ended with an error at `limit` bytes or at `end`: a gzip bomb
/// or a slow member costs the stage a bounded amount of work, whatever the tar says.
struct Bounded<R> {
    inner: R,
    left: u64,
    end: Instant,
    tripped: Rc<Cell<Option<Trip>>>,
}

impl<R: Read> Read for Bounded<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let stop = |why| {
            self.tripped.set(Some(why));
            Err(io::Error::other("the unpacking was stopped"))
        };
        if Instant::now() >= self.end {
            return stop(Trip::Time);
        }
        let n = self.inner.read(buf)?;
        match self.left.checked_sub(n as u64) {
            Some(left) => self.left = left,
            None => return stop(Trip::Size),
        }
        Ok(n)
    }
}

/// Write the one file `tarball` holds, which must be named `name`, to `dest` (a new file,
/// mode 0755). `cap` bounds the file, and (with a little framing) everything the gzip
/// stream decompresses to; the whole is done within `within`. What follows the tar's end
/// is only zero bytes, and nothing follows the gzip member: a second member or any
/// trailing byte refuses the archive, since a reader that stops early would not see it.
pub fn unpack_one(
    tarball: &Path,
    name: &str,
    dest: &Path,
    cap: u64,
    within: Duration,
) -> Result<(), ArchiveFault> {
    let tripped = Rc::new(Cell::new(None));
    let bad = |e: io::Error| match tripped.get() {
        Some(Trip::Size) => ArchiveFault::Oversize { cap },
        Some(Trip::Time) => ArchiveFault::TimedOut,
        None => ArchiveFault::NotTarGz(e.to_string()),
    };
    let mut compressed = BufReader::new(File::open(tarball).map_err(bad)?);
    let decoded = Bounded {
        inner: GzDecoder::new(&mut compressed),
        left: cap.saturating_add(FRAMING),
        end: Instant::now() + within,
        tripped: Rc::clone(&tripped),
    };
    let mut archive = Archive::new(decoded);
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
    if seen != 1 {
        return Err(ArchiveFault::Entries(seen));
    }
    // The rest of the decompressed stream must be the tar's zero padding, read to the end
    // so that the gzip trailer (its length and checksum) is checked too.
    let mut rest = archive.into_inner();
    let mut block = [0u8; 8192];
    loop {
        let n = rest.read(&mut block).map_err(bad)?;
        if n == 0 {
            break;
        }
        if block[..n].iter().any(|b| *b != 0) {
            return Err(ArchiveFault::TrailingData);
        }
    }
    drop(rest);
    // And nothing after the gzip member: not a second member, not a byte.
    if !compressed.fill_buf().map_err(bad)?.is_empty() {
        return Err(ArchiveFault::TrailingData);
    }
    Ok(())
}

#[cfg(test)]
#[path = "archive_tests.rs"]
mod tests;
