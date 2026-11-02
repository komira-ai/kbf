//! Unpacking a component's tarball: the members the caller names, each a regular file.
//!
//! A release artifact is `kbf-<commit>-<platform>.tar.gz`. It holds the binary, named for
//! the component (`kbf`, or on Linux `kbf-daemon`), with no directory in front of it, and
//! it may also hold the component's updater declaration, [`declaration_member`]
//! (`updater.d/<component>.toml`). This is only ever done to a tarball the attestation
//! already accepted, and it still checks every member, wanted or not: a member whose name
//! is absolute or has a `..` component, and a link, a device or a FIFO, refuse the whole
//! archive, naming the member. A member the caller did not ask for is read past, never
//! written, and logged once; a member the caller requires and the archive does not hold
//! refuses it. A wanted member's name, type and size are checked before any of its bytes is
//! written, and at most its cap's bytes are. A refusal can come after an earlier member was
//! written: the caller unpacks into its own scratch directory, which goes with the refusal.

use std::cell::Cell;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use flate2::bufread::GzDecoder;
use tar::{Archive, EntryType};

use crate::refusal::ArchiveFault;

/// What the tar framing may add to the decompressed stream (the headers, the padding and
/// the zero blocks at the end), beyond the members' own caps.
const FRAMING: u64 = 64 * 1024;

/// The directory a release tarball carries its updater declarations in, and the directory
/// they are unpacked into under the release's own: the name of the host's declaration
/// directory, `/etc/updater.d` (`update_rights::load::DECLARATION_DIR`).
pub const DECLARATIONS: &str = "updater.d";

/// The most an updater declaration may be.
pub const DECLARATION_CAP: u64 = 64 * 1024;

/// The member name of the updater declaration of the component `name`, which a release may
/// carry beside the binary: `updater.d/<name>.toml`.
pub fn declaration_member(name: &str) -> String {
    format!("{DECLARATIONS}/{name}.toml")
}

/// One member the caller wants: its exact name in the tarball, the new file it is written
/// to (with its mode), the most it may be, and whether the archive must hold it.
#[derive(Clone, Copy, Debug)]
pub struct Member<'a> {
    pub name: &'a str,
    pub dest: &'a Path,
    pub mode: u32,
    pub cap: u64,
    pub required: bool,
}

/// What an unpack took and what it passed over.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Unpacked {
    /// The names of the wanted members written, in the archive's order.
    pub taken: Vec<String>,
    /// The names of the members no one asked for, in the archive's order.
    pub ignored: Vec<String>,
}

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

/// The refusal of a member whose name could write outside the directory it is unpacked
/// into: empty, absolute, or with a `..` component.
fn check_name(name: &[u8]) -> Result<(), ArchiveFault> {
    let escapes = name.is_empty()
        || name.starts_with(b"/")
        || name.split(|b| *b == b'/').any(|part| part == b"..");
    if escapes {
        return Err(ArchiveFault::UnsafePath(
            String::from_utf8_lossy(name).into_owned(),
        ));
    }
    Ok(())
}

/// Make the directory `dir` (0755) in its parent, which must be there, unless it is there
/// already as a directory. Never through a link: a link (or anything but a directory) in
/// its place is refused, so nothing is written outside the directory unpacked into.
fn make_dir(dir: &Path) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o755).create(dir) {
        Err(e)
            if e.kind() == io::ErrorKind::AlreadyExists && fs::symlink_metadata(dir)?.is_dir() =>
        {
            Ok(())
        }
        made => made,
    }
}

/// What a member of a kind that is never unpacked is called, or `None` for a regular file
/// and a directory (which is only ever passed over: no wanted member is one).
fn refused_kind(kind: EntryType) -> Option<&'static str> {
    match kind {
        EntryType::Regular | EntryType::Directory => None,
        EntryType::Symlink => Some("symbolic link"),
        EntryType::Link => Some("hard link"),
        EntryType::Char | EntryType::Block => Some("device"),
        EntryType::Fifo => Some("FIFO"),
        _ => Some("member of another kind"),
    }
}

/// Write each of `members` that `tarball` holds to its `dest` (a new file; its directory is
/// made, 0755, when it is not there, in a parent that is: [`make_dir`]). Every member is
/// checked ([`check_name`], [`refused_kind`]); a wanted one must be a regular file, at most its `cap`, and there once.
/// The whole stream decompresses to at most the members' caps and some framing, and is
/// done within `within`. What follows the tar's end is only zero bytes, and nothing follows
/// the gzip member: a second member or any trailing byte refuses the archive, since a
/// reader that stops early would not see it. A member no one asked for is logged and
/// listed in [`Unpacked::ignored`].
pub fn unpack_members(
    tarball: &Path,
    members: &[Member<'_>],
    within: Duration,
) -> Result<Unpacked, ArchiveFault> {
    let caps = members
        .iter()
        .fold(0u64, |sum, m| sum.saturating_add(m.cap));
    let tripped = Rc::new(Cell::new(None));
    let bad = |e: io::Error| match tripped.get() {
        Some(Trip::Size) => ArchiveFault::Oversize { cap: caps },
        Some(Trip::Time) => ArchiveFault::TimedOut,
        None => ArchiveFault::NotTarGz(e.to_string()),
    };
    let mut compressed = BufReader::new(File::open(tarball).map_err(bad)?);
    let decoded = Bounded {
        inner: GzDecoder::new(&mut compressed),
        left: caps.saturating_add(FRAMING),
        end: Instant::now() + within,
        tripped: Rc::clone(&tripped),
    };
    let mut archive = Archive::new(decoded);
    let mut seen = 0usize;
    let mut unpacked = Unpacked::default();
    for entry in archive.entries().map_err(bad)? {
        let mut entry = entry.map_err(bad)?;
        seen += 1;
        let name_bytes = entry.path_bytes().into_owned();
        check_name(&name_bytes)?;
        let name = String::from_utf8_lossy(&name_bytes).into_owned();
        let kind = entry.header().entry_type();
        if let Some(kind) = refused_kind(kind) {
            return Err(ArchiveFault::NotRegular { name, kind });
        }
        let Some(member) = members.iter().find(|m| m.name.as_bytes() == name_bytes) else {
            // Not asked for: read past (its bytes count against the bound), never written.
            tracing::info!(member = ?name, "a member of the tarball not taken here: ignored");
            unpacked.ignored.push(name);
            continue;
        };
        if kind != EntryType::Regular {
            return Err(ArchiveFault::NotRegular {
                name,
                kind: "directory",
            });
        }
        if unpacked.taken.contains(&name) {
            return Err(ArchiveFault::Duplicate(name));
        }
        if entry.size() > member.cap {
            return Err(ArchiveFault::Oversize { cap: member.cap });
        }
        make_dir(member.dest.parent().unwrap_or(Path::new(""))).map_err(bad)?;
        let mut out = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(member.mode)
            .open(member.dest)
            .map_err(bad)?;
        // Whatever the umask said, the file has the member's mode. The reader gives the
        // header's size and no more, and the header was checked.
        out.set_permissions(fs::Permissions::from_mode(member.mode))
            .and_then(|()| io::copy(&mut (&mut entry).take(member.cap), &mut out))
            .and_then(|_| out.sync_all())
            .map_err(bad)?;
        unpacked.taken.push(name);
    }
    if seen == 0 {
        return Err(ArchiveFault::Entries(0));
    }
    if let Some(missing) = members
        .iter()
        .find(|m| m.required && !unpacked.taken.iter().any(|t| t == m.name))
    {
        return Err(ArchiveFault::WrongName(missing.name.to_owned()));
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
    Ok(unpacked)
}

#[cfg(test)]
#[path = "archive_tests.rs"]
mod tests;
