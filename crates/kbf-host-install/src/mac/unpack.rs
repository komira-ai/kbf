//! Unpacking a release tarball on the Mac: the members the stage names, each a regular file.
//!
//! The rules are those of the Linux installer's `kbf_installer::archive` (which this side
//! cannot link): the stage asks for the binary `kbf` and, when the release carries one, its
//! updater declaration [`declaration_member`] (`updater.d/kbf.toml`). Every member is
//! checked by the name the tar reader resolves (a GNU long name, a PAX `path`, a ustar
//! prefix), wanted or not: a name that is empty, absolute or has a `..` component, and a
//! link, a device, a FIFO or another special member, refuse the archive, naming the member.
//! A member no one asked for is read past, never written, and told on the log as it is
//! passed over. The whole decompressed stream is bounded and read to its end: what follows
//! the tar's end is only zero bytes, and nothing follows the gzip member.

use std::cell::Cell;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::rc::Rc;

use flate2::bufread::GzDecoder;

use super::layout::BINARY;

/// The directory a release tarball carries its updater declarations in, and the one they
/// are unpacked into in the stage: the name of the host's declaration directory,
/// `/etc/updater.d` (`update_rights::load::DECLARATION_DIR`).
pub(crate) const DECLARATIONS: &str = "updater.d";

/// The most an updater declaration may be.
pub(crate) const DECLARATION_CAP: u64 = 64 * 1024;

/// What the tar framing may add to the decompressed stream (the headers, the padding and
/// the zero blocks at the end), beyond the members' own caps.
pub(crate) const FRAMING: u64 = 64 * 1024;

/// The member name of the updater declaration of the binary `name`, which a release may
/// carry beside it: `updater.d/<name>.toml`.
pub(crate) fn declaration_member(name: &str) -> String {
    format!("{DECLARATIONS}/{name}.toml")
}

/// One member the stage wants from the tarball: its exact name, the new file it is written
/// to (with its mode), the most it may be, and whether the tarball must hold it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Wanted<'a> {
    pub name: &'a str,
    pub dest: &'a Path,
    pub mode: u32,
    pub cap: u64,
    pub required: bool,
}

/// The decompressed stream, ended with an error past `left` bytes: a member no one asked
/// for is read past, and a gzip bomb in it costs a bounded amount of work.
struct Bounded<R> {
    inner: R,
    left: u64,
    tripped: Rc<Cell<bool>>,
}

impl<R: Read> Read for Bounded<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        match self.left.checked_sub(n as u64) {
            Some(left) => self.left = left,
            None => {
                self.tripped.set(true);
                return Err(io::Error::other("the unpacking was stopped"));
            }
        }
        Ok(n)
    }
}

/// The binary, and its declaration when the release carries one, unpacked into `stage`; each
/// member passed over is told on `log`, one line each, as it is passed over.
pub(crate) fn unpack_release(
    tarball: &Path,
    stage: &Path,
    binary_max: u64,
    log: &mut dyn Write,
) -> Result<(), String> {
    let declaration = declaration_member(BINARY);
    let wanted = [
        Wanted {
            name: BINARY,
            dest: &stage.join(BINARY),
            mode: 0o755,
            cap: binary_max,
            required: true,
        },
        Wanted {
            name: &declaration,
            dest: &stage.join(&declaration),
            mode: 0o644,
            cap: DECLARATION_CAP,
            required: false,
        },
    ];
    unpack(tarball, &wanted, log).map(drop)
}

/// The line told for a member passed over.
pub(crate) fn ignored_line(name: &str) -> String {
    format!("kbf-host-install: the release tarball's member {name:?} is not taken: ignored")
}

/// Make the directory `dir` in its parent, which must be there, mode 0755 whatever the umask,
/// unless it is there already as a directory, which is the caller's and is left as it is.
/// Never through a link: a link (or anything but a directory) in its place is refused, so
/// nothing is written outside the directory unpacked into.
fn make_dir(dir: &Path) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o755).create(dir) {
        Ok(()) => fs::set_permissions(dir, fs::Permissions::from_mode(0o755)),
        Err(e)
            if e.kind() == io::ErrorKind::AlreadyExists && fs::symlink_metadata(dir)?.is_dir() =>
        {
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// What a member of a kind that is never unpacked is called, or `None` for a regular file
/// and a directory (which is only ever passed over: no wanted member is one).
fn refused_kind(kind: tar::EntryType) -> Option<&'static str> {
    match kind {
        tar::EntryType::Regular | tar::EntryType::Directory => None,
        tar::EntryType::Symlink => Some("symbolic link"),
        tar::EntryType::Link => Some("hard link"),
        tar::EntryType::Char | tar::EntryType::Block => Some("device"),
        tar::EntryType::Fifo => Some("FIFO"),
        _ => Some("member of another kind"),
    }
}

/// Write each of `wanted` that the gzipped tar at `tarball` holds to its `dest` (a new file;
/// its directory is made when it is not there: [`make_dir`]), and return the names of the
/// members no one asked for, each told on `log` as it is passed over. The request names
/// each member and each destination once (refused before the tarball is opened otherwise).
/// A wanted member must be a regular file, at most its cap, and there once; a required one
/// the tarball does not hold refuses it by name, before an archive with no entries at all
/// is refused. The whole stream decompresses to at most the caps and [`FRAMING`].
pub(crate) fn unpack(
    tarball: &Path,
    wanted: &[Wanted<'_>],
    log: &mut dyn Write,
) -> Result<Vec<String>, String> {
    for (i, want) in wanted.iter().enumerate() {
        if wanted[..i]
            .iter()
            .any(|w| w.name == want.name || w.dest == want.dest)
        {
            return Err(format!(
                "the unpack asks for {:?} by a name or a destination it already asks for",
                want.name
            ));
        }
    }
    let caps = wanted.iter().fold(0u64, |sum, w| sum.saturating_add(w.cap));
    let tripped = Rc::new(Cell::new(false));
    let mut compressed = BufReader::new(File::open(tarball).map_err(|e| e.to_string())?);
    let decoded = Bounded {
        inner: GzDecoder::new(&mut compressed),
        left: caps.saturating_add(FRAMING),
        tripped: Rc::clone(&tripped),
    };
    let bad = |e: io::Error| {
        if tripped.get() {
            format!(
                "it decompresses to more than {} bytes",
                caps.saturating_add(FRAMING)
            )
        } else {
            e.to_string()
        }
    };
    let mut archive = tar::Archive::new(decoded);
    let mut taken: Vec<String> = Vec::new();
    let mut ignored = Vec::new();
    let mut seen = 0usize;
    for entry in archive.entries().map_err(bad)? {
        let mut entry = entry.map_err(bad)?;
        seen += 1;
        let raw = entry.path_bytes().into_owned();
        let name = String::from_utf8_lossy(&raw).into_owned();
        let escapes = raw.is_empty()
            || raw.starts_with(b"/")
            || raw.split(|b| *b == b'/').any(|part| part == b"..");
        if escapes {
            return Err(format!(
                "the member {name:?} leaves the directory it is unpacked into"
            ));
        }
        let kind = entry.header().entry_type();
        if let Some(what) = refused_kind(kind) {
            return Err(format!(
                "the member {name:?} is a {what}, not a regular file"
            ));
        }
        let Some(want) = wanted.iter().find(|w| w.name.as_bytes() == raw) else {
            // A log that cannot be written loses the line, not the release.
            let _ = writeln!(log, "{}", ignored_line(&name));
            ignored.push(name);
            continue;
        };
        if kind != tar::EntryType::Regular {
            return Err(format!(
                "the member {name:?} is a directory, not a regular file"
            ));
        }
        if taken.contains(&name) {
            return Err(format!("the member {name:?} is in it twice"));
        }
        let size = entry.size();
        if size > want.cap {
            return Err(format!(
                "the member {name:?} is larger than {} bytes",
                want.cap
            ));
        }
        make_dir(want.dest.parent().unwrap_or(Path::new(""))).map_err(|e| e.to_string())?;
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(want.mode)
            .open(want.dest)
            .map_err(|e| e.to_string())?;
        // Whatever the umask said, the file has the member's mode.
        out.set_permissions(fs::Permissions::from_mode(want.mode))
            .and_then(|()| io::copy(&mut (&mut entry).take(size), &mut out))
            .map_err(bad)?;
        taken.push(name);
    }
    if let Some(missing) = wanted
        .iter()
        .find(|w| w.required && !taken.iter().any(|t| t == w.name))
    {
        return Err(format!("holds no member named {}", missing.name));
    }
    if seen == 0 {
        return Err("holds no file".to_owned());
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
            return Err("data follows the end of the archive".to_owned());
        }
    }
    drop(rest);
    // And nothing after the gzip member: not a second member, not a byte.
    if !compressed.fill_buf().map_err(bad)?.is_empty() {
        return Err("data follows the end of the archive".to_owned());
    }
    Ok(ignored)
}
