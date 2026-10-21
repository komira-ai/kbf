//! What keeps one process on one state file: the lock beside it, and the refusal of a
//! filesystem SQLite cannot be trusted on.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

use super::SqliteError;

/// Filesystem type numbers (`statfs` `f_type`) of network filesystems, with their names.
const NETWORK_FILESYSTEMS: &[(u32, &str)] = &[
    (0x0000_6969, "NFS"),
    (0x0000_517B, "SMB"),
    (0xFF53_4D42, "CIFS"),
    (0xFE53_4D42, "SMB2"),
    (0x00C3_6400, "CephFS"),
    (0x0102_1997, "9p"),
    (0x5346_414F, "AFS"),
    (0x7375_7245, "Coda"),
    (0x0BD0_0BD0, "Lustre"),
    (0x4750_4653, "GPFS"),
    (0x7461_636F, "OCFS2"),
];

/// How many symbolic links [`canonical_db`] follows from one name, as Linux does.
const MAX_LINKS: usize = 40;

/// The lock file of the state file `db`: its name with `.lock` after it.
pub(super) fn lock_path(db: &Path) -> PathBuf {
    let mut name: OsString = db.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// The one name of the state file `db`, whichever spelling of it is given: its directory
/// with every link and `..` resolved, and its own name, followed while it is a symbolic
/// link (to a file that may not exist yet). Every name of one file gives the same path,
/// so every process that opens it agrees on its lock.
///
/// # Errors
/// The directory cannot be resolved (it does not exist), a link cannot be read, or more
/// than [`MAX_LINKS`] links follow one another (a loop).
pub(super) fn canonical_db(db: &Path) -> io::Result<PathBuf> {
    let mut path = db.to_owned();
    for _ in 0..=MAX_LINKS {
        let dir = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.canonicalize()?,
            _ => Path::new(".").canonicalize()?,
        };
        // No name (`/`, `..`): not a file, and whatever it is has no links left.
        let Some(name) = path.file_name() else {
            return path.canonicalize();
        };
        let full = dir.join(name);
        match fs::symlink_metadata(&full) {
            Ok(meta) if meta.file_type().is_symlink() => path = dir.join(fs::read_link(&full)?),
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => return Ok(full),
        }
    }
    Err(io::Error::other(format!(
        "{}: more than {MAX_LINKS} symbolic links follow one another",
        db.display()
    )))
}

/// The lock file of the state file `db`, by its one name ([`canonical_db`]).
fn canonical_lock(db: &Path) -> io::Result<PathBuf> {
    canonical_db(db).map(|file| lock_path(&file))
}

/// The name of the network filesystem with type number `magic`, if it is one.
pub(super) fn network_filesystem(magic: u32) -> Option<&'static str> {
    for (number, name) in NETWORK_FILESYSTEMS {
        if *number == magic {
            return Some(*name);
        }
    }
    None
}

/// The type number of the filesystem `dir` is on.
#[cfg(target_os = "linux")]
pub(super) fn filesystem_type(dir: &Path) -> io::Result<u32> {
    let stat = rustix::fs::statfs(dir)?;
    // The field is a signed word on 64-bit targets; the numbers above fit 32 bits.
    Ok(stat.f_type as u32)
}

/// The type number of the filesystem `dir` is on: not known off Linux, so the network
/// filesystem check is a Linux-only check and no other system refuses anything.
#[cfg(not(target_os = "linux"))]
pub(super) fn filesystem_type(_dir: &Path) -> io::Result<u32> {
    Ok(0)
}

/// Refuses `db` when the directory it lives in is on a network filesystem, as `probe` (the
/// filesystem type of a directory) tells.
pub(super) fn refuse_network_filesystem(
    db: &Path,
    probe: &dyn Fn(&Path) -> io::Result<u32>,
) -> Result<(), SqliteError> {
    let dir = match db.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    match network_filesystem(probe(dir)?) {
        Some(fs) => Err(SqliteError::NetworkFs {
            path: db.to_owned(),
            fs,
        }),
        None => Ok(()),
    }
}

/// Refuses `db` when the file has more than one name: the lock is beside one name, so a
/// process that opened the file by another would lock elsewhere. A file that does not
/// exist yet has one name (the one it is created under).
pub(super) fn refuse_hard_link(db: &Path) -> Result<(), SqliteError> {
    use std::os::unix::fs::MetadataExt as _;
    match fs::metadata(db) {
        Ok(meta) if meta.is_file() && meta.nlink() > 1 => Err(SqliteError::HardLinked {
            path: db.to_owned(),
            links: meta.nlink(),
        }),
        // Absent, or not a file: the open that follows says what is wrong.
        _ => Ok(()),
    }
}

/// A lock file with its `flock` held, released when this is dropped (or the process ends).
///
/// An `flock` belongs to the open file description, not to the descriptor, and a
/// description lives until every copy of the descriptor is closed. A process forked while
/// the lock is held (any spawn of a child by another thread of this process) carries a
/// copy until it execs, so closing the file would leave the state file locked for as
/// long as that takes. Dropping unlocks first, which no copy outlives, so the state file
/// can be opened again once the drop returns.
#[derive(Debug)]
pub(super) struct LockFile(File);

impl LockFile {
    /// Takes an exclusive lock on `file` without waiting.
    fn try_take(file: File) -> Result<LockFile, TryLockError> {
        file.try_lock().map(|()| LockFile(file))
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        // An error leaves the close that follows to release the lock, as before.
        let _ = self.0.unlock();
    }
}

/// Creates `db`'s lock file if missing and takes an exclusive `flock` on it; returns it
/// and its name. The lock file is beside the file's one name ([`canonical_db`]), so a
/// link to the file, or a linked directory, names the same lock. Dropping the lock, or
/// the process ending, releases it.
pub(super) fn take_lock(db: &Path) -> Result<(LockFile, PathBuf), SqliteError> {
    let lock = canonical_lock(db)?;
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&lock)?;
    match LockFile::try_take(file) {
        Ok(taken) => Ok((taken, lock)),
        Err(error) => Err(lock_failed(db, lock, error)),
    }
}

/// The lock a [`super::SqliteStore`] holds on its state file for as long as it is open,
/// taken instead by a tool that works on the file while no store may have it (a
/// restore). It is the same `flock` on the same `<file>.lock` (beside the file's one
/// name, whichever name of it the store or the tool was given), so a store cannot open
/// the file while this is held, and this cannot be taken while a store is open.
#[derive(Debug)]
pub struct StoreLock {
    db: PathBuf,
    lock: PathBuf,
    _file: LockFile,
}

impl StoreLock {
    /// Takes the lock of the state file `db`, creating the lock file if missing, after the
    /// same network filesystem check a store makes.
    ///
    /// # Errors
    /// [`SqliteError::Locked`] while a process (a store) holds it;
    /// [`SqliteError::NetworkFs`] on a network filesystem; otherwise the failing call.
    pub fn take(db: &Path) -> Result<StoreLock, SqliteError> {
        refuse_network_filesystem(db, &filesystem_type)?;
        refuse_hard_link(db)?;
        let (file, lock) = take_lock(db)?;
        Ok(StoreLock {
            db: db.to_owned(),
            lock,
            _file: file,
        })
    }

    /// Whether a process holds the lock of the state file `db` now, found without
    /// creating or changing anything: an absent lock file is a lock nobody holds (a store
    /// creates it before it opens the file). The answer can be stale the moment it is
    /// given, so only a plan uses it; a change takes the lock. A directory that does not
    /// exist has no lock file either.
    ///
    /// # Errors
    /// The file's name cannot be resolved, the lock file cannot be opened, or the lock
    /// call failed.
    pub fn is_held(db: &Path) -> Result<bool, SqliteError> {
        probe_lock(db, || ())
    }

    /// The state file this locks, as the caller named it.
    #[must_use]
    pub fn db(&self) -> &Path {
        &self.db
    }

    /// The lock file: the state file's one name with `.lock` after it.
    #[must_use]
    pub fn lock_file(&self) -> &Path {
        &self.lock
    }
}

/// [`StoreLock::is_held`], calling `probing` once the probe has tried the lock, so while
/// the probe holds it when it took it: the instant in which a process forked by another
/// thread would carry a copy of the probe's descriptor, which a test makes happen there.
pub(super) fn probe_lock(db: &Path, probing: impl FnOnce()) -> Result<bool, SqliteError> {
    let opened = canonical_lock(db).and_then(|lock| Ok((File::open(&lock)?, lock)));
    match opened {
        Ok((file, lock)) => {
            let attempt = LockFile::try_take(file);
            probing();
            // The probe lets go as soon as it has its answer: consuming the lock drops it.
            held(db, lock, attempt.map(drop))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(SqliteError::Io(format!(
            "the lock of {}: {error}",
            db.display()
        ))),
    }
}

/// What a probing `try_lock` says: taken (and released again at once) means nobody holds
/// it, `WouldBlock` that someone does; any other failure is an error.
pub(super) fn held(
    db: &Path,
    lock: PathBuf,
    attempt: Result<(), TryLockError>,
) -> Result<bool, SqliteError> {
    match attempt {
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(error) => Err(lock_failed(db, lock, error)),
    }
}

/// What a refused `try_lock` means: `Locked` when another descriptor holds the lock (the
/// one case that means "another process is using this file"), and an I/O error for any
/// other failure of the call, which says nothing about who holds the file.
pub(super) fn lock_failed(db: &Path, lock: PathBuf, error: TryLockError) -> SqliteError {
    match error {
        TryLockError::WouldBlock => SqliteError::Locked {
            path: db.to_owned(),
            lock,
            source: io::Error::from(io::ErrorKind::WouldBlock),
        },
        TryLockError::Error(source) => {
            SqliteError::Io(format!("could not take {}: {source}", lock.display()))
        }
    }
}
