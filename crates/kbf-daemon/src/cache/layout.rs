//! The cache directory on disk (`docs/design/worker-cache.md#61-on-disk-layout`): the
//! keys and their names, the lock, and what start-up clears before anything is read.
//!
//! ```text
//! <cache-dir>/lock                       exclusive flock; never unlinked
//! <cache-dir>/v1/blobs/<h0h1>/<sha256>-<size>[.x]
//! <cache-dir>/v1/tmp/<n>                 fills in progress; cleared at start
//! <cache-dir>/v1/index.snap              the index (`index`)
//! ```

use std::fs::File;
use std::io::{self, ErrorKind};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use kbf_proto::reapi::Digest;
use rustix::fs::FlockOperation;

use super::Disabled;

/// Every entry's modification time, in Unix seconds: one fixed moment in the past, so
/// an input's mtime is a function of the input tree alone. It is later than 1980, the
/// earliest time a zip archive can record, so tools that pack inputs accept it.
pub const FIXED_MTIME: i64 = 1_790_000_000;

/// The layout version this daemon reads and writes.
pub(crate) const VERSION: &str = "v1";

/// What one cache entry holds: a blob, and whether its files are executable. Hard links
/// share their mode bits, so the executable and the plain copy of one blob are two
/// entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key {
    pub hash: [u8; 32],
    pub size: u64,
    pub executable: bool,
}

impl Key {
    /// The key of `digest` (a lower-case SHA-256 and a size), or `None` for a digest
    /// that names no SHA-256 blob.
    #[must_use]
    pub fn of(digest: &Digest, executable: bool) -> Option<Self> {
        let size = u64::try_from(digest.size_bytes).ok()?;
        let mut hash = [0; 32];
        hex::decode_to_slice(&digest.hash, &mut hash).ok()?;
        let key = Self {
            hash,
            size,
            executable,
        };
        // Upper-case hex decodes too, but names another resource.
        (hex::encode(hash) == digest.hash).then_some(key)
    }

    /// The digest this key's bytes have.
    #[must_use]
    pub fn digest(&self) -> Digest {
        Digest {
            hash: hex::encode(self.hash),
            // Every key is made from a digest's `i64` size.
            size_bytes: self.size as i64,
        }
    }

    /// The other key of the same blob: executable for a plain one, and back.
    #[must_use]
    pub fn sibling(&self) -> Self {
        Self {
            executable: !self.executable,
            ..*self
        }
    }

    /// The entry's file name: `<sha256>-<size>`, with `.x` when executable.
    #[must_use]
    pub fn name(&self) -> String {
        let x = if self.executable { ".x" } else { "" };
        format!("{}-{}{x}", hex::encode(self.hash), self.size)
    }

    /// The directory under `blobs/` the entry is in: its first two hex digits.
    #[must_use]
    pub fn prefix(&self) -> String {
        hex::encode(&self.hash[..1])
    }

    /// The key a file named `name` holds, or `None` if `name` is not exactly the name
    /// of one.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        let rest = name.strip_suffix(".x").unwrap_or(name);
        let (hash, size) = rest.split_once('-')?;
        let digest = Digest {
            hash: hash.to_owned(),
            size_bytes: size.parse().ok()?,
        };
        let key = Self::of(&digest, rest.len() < name.len())?;
        (key.name() == name).then_some(key)
    }

    /// The mode an entry of this key has.
    #[must_use]
    pub fn mode(&self) -> u32 {
        if self.executable { 0o555 } else { 0o444 }
    }
}

/// The paths of an opened cache, the lock it holds, and what start-up found.
#[derive(Debug)]
pub(crate) struct Layout {
    /// `<cache-dir>/v1`.
    pub v1: PathBuf,
    pub blobs: PathBuf,
    pub tmp: PathBuf,
    /// The open lock file, holding the exclusive flock for as long as it is open.
    pub lock: File,
    /// Whether the cache shares a filesystem with the scratch directory.
    pub same_filesystem: bool,
}

/// Takes the lock on `dir`, then (and only then) moves an unknown layout version aside,
/// makes `v1/` and its directories, and clears `tmp/` and `index.snap.tmp`. `now` names
/// what is moved aside. Nothing under `dir` but the lock file is touched when the lock
/// is not taken, and the lock file is never removed.
///
/// # Errors
/// [`Disabled::Locked`] when another process holds the lock; [`Disabled::Unusable`]
/// when the directory is missing, cannot be written, or `v1` is not a directory.
pub(crate) fn open(dir: &Path, scratch: &Path, now: u64) -> Result<Layout, Disabled> {
    let lock_path = dir.join("lock");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .map_err(unusable(&lock_path))?;
    let locked = rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive);
    if locked == Err(rustix::io::Errno::WOULDBLOCK) {
        return Err(Disabled::Locked);
    }
    locked
        .map_err(io::Error::from)
        .map_err(unusable(&lock_path))?;
    move_aside(dir, now).map_err(unusable(dir))?;
    let v1 = dir.join(VERSION);
    let blobs = v1.join("blobs");
    let tmp = v1.join("tmp");
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700).recursive(true);
    let prefixes = (0..=u8::MAX).map(|byte| blobs.join(hex::encode([byte])));
    for made in [v1.clone(), tmp.clone()].into_iter().chain(prefixes) {
        builder.create(&made).map_err(unusable(&made))?;
    }
    clear(&tmp).map_err(unusable(&tmp))?;
    let snapshot_tmp = v1.join(super::index::SNAPSHOT_TMP);
    remove_if_present(&snapshot_tmp).map_err(unusable(&snapshot_tmp))?;
    let same_filesystem = Some(device(&v1).map_err(unusable(&v1))?) == device(scratch).ok();
    Ok(Layout {
        v1,
        blobs,
        tmp,
        lock,
        same_filesystem,
    })
}

fn unusable(what: &Path) -> impl FnOnce(io::Error) -> Disabled + '_ {
    move |e| Disabled::Unusable(format!("{}: {e}", what.display()))
}

/// Renames each other layout version under `dir` (`v<digits>`, not `v1`) to
/// `<name>.moved-<now>`, so it is never read and never mistaken for this one.
fn move_aside(dir: &Path, now: u64) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let name = entry?.file_name();
        let digits = name.as_bytes().strip_prefix(b"v").unwrap_or_default();
        let version = !digits.is_empty() && digits.iter().all(u8::is_ascii_digit);
        if version && name != VERSION {
            let name = name.to_string_lossy();
            let to = format!("{name}.moved-{now}");
            tracing::warn!("cache: moving layout {name} aside to {to}; it is never read");
            std::fs::rename(dir.join(&*name), dir.join(to))?;
        }
    }
    Ok(())
}

/// Removes every file in `dir` (the fills a previous run left in `tmp/`).
fn clear(dir: &Path) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        std::fs::remove_file(entry?.path())?;
    }
    Ok(())
}

/// Removes the file `path`; one that is not there is not an error.
pub(crate) fn remove_if_present(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// The device of `path`, or of its nearest ancestor that exists: the scratch directory
/// may be made only with the first lease.
fn device(path: &Path) -> io::Result<u64> {
    let mut at = path;
    loop {
        match std::fs::metadata(at) {
            Err(e) if e.kind() == ErrorKind::NotFound => at = at.parent().ok_or(e)?,
            other => return other.map(|m| m.dev()),
        }
    }
}

/// Whether `name`, a cache directory's last component, is one a lease sweep removes
/// (the container driver's `kbf-lease-`, the native driver's `lease-`).
#[must_use]
pub(crate) fn swept_name(name: &Path) -> bool {
    let name = name.as_os_str().as_bytes();
    name.starts_with(b"kbf-lease-") || name.starts_with(b"lease-")
}

/// The most hard links a file under `dir` may have (`pathconf(_PC_LINK_MAX)`), or
/// ext4's 65,000 when the system does not say.
pub(crate) fn link_max(dir: &Path) -> u64 {
    let max = std::ffi::CString::new(dir.as_os_str().as_bytes()).map_or(-1, |path| {
        // SAFETY: `path` is a NUL-terminated string that lives across the call, and
        // `pathconf` only reads it. rustix offers no `pathconf`.
        unsafe { libc::pathconf(path.as_ptr(), libc::_PC_LINK_MAX) }
    });
    u64::try_from(max).ok().filter(|&m| m > 0).unwrap_or(65_000)
}
