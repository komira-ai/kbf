//! What keeps one process on one state file: the lock beside it, and the refusal of a
//! filesystem SQLite cannot be trusted on.

use std::ffi::OsString;
use std::fs::{File, OpenOptions, TryLockError};
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

/// The lock file of the state file `db`: its name with `.lock` after it.
pub(super) fn lock_path(db: &Path) -> PathBuf {
    let mut name: OsString = db.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
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

/// Creates `db`'s lock file if missing and takes an exclusive `flock` on it. The lock
/// goes with the descriptor: dropping the file, or the process ending, releases it.
pub(super) fn take_lock(db: &Path) -> Result<File, SqliteError> {
    let lock = lock_path(db);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
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
