//! The frontend's single-writer lock.
//!
//! A frontend holds an exclusive `flock` on `<state file>.lock`, beside the file's one name
//! (every link resolved), for as long as it has the state file open. This tool takes the
//! same lock on the same name: it cannot be taken while a frontend runs, and a frontend
//! cannot start while it is held. The lock file is only ever opened, never created or
//! written: a frontend creates it before it opens the state file, so a state file with no
//! lock file beside it has never been opened by one, and there is nothing to take.
//!
//! The probe is written here and not borrowed from the SQLite store of `kbf-state`, so this
//! crate does not depend on that store's code; a test holds the lock with that store to show
//! that the two agree.

use std::fs::{self, File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

use crate::error::Error;

/// `path` with `suffix` after its name: the files a frontend and SQLite keep beside a state
/// file.
#[must_use]
pub fn beside(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// The one name of the state file `db`, with every link resolved as the frontend resolves
/// them, so that both take the lock beside the same name.
///
/// # Errors
/// [`Error::Failed`] when the file does not exist or a link cannot be resolved.
pub fn canonical(db: &Path) -> Result<PathBuf, Error> {
    fs::canonicalize(db).map_err(|err| Error::Failed(format!("{}: {err}", db.display())))
}

/// A lock held until the guard is dropped, or nothing when there is no lock file.
#[derive(Debug)]
pub struct Guard(Option<File>);

impl Drop for Guard {
    fn drop(&mut self) {
        // Unlocked before the descriptor closes: a child forked in the meantime holds a
        // copy of the descriptor, and the lock would last as long as the copy does.
        if let Some(file) = &self.0 {
            let _ = file.unlock();
        }
    }
}

/// Takes the lock of the state file `db` (its canonical name) without waiting.
///
/// # Errors
/// [`Error::Refused`] while a process holds the lock; [`Error::Failed`] when the lock file
/// cannot be opened or the lock call fails.
pub fn take(db: &Path) -> Result<Guard, Error> {
    let lock = beside(db, ".lock");
    let file = match File::open(&lock) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Guard(None)),
        Err(err) => return Err(Error::Failed(format!("{}: {err}", lock.display()))),
    };
    judged(&lock, file.try_lock())?;
    Ok(Guard(Some(file)))
}

/// What a try-lock on `lock` says: taken is fine, `WouldBlock` is a running frontend, and
/// any other failure says nothing about who holds the file.
///
/// # Errors
/// [`Error::Refused`] for `WouldBlock`, [`Error::Failed`] for any other failure.
pub fn judged(lock: &Path, attempt: Result<(), TryLockError>) -> Result<(), Error> {
    let name = lock.display();
    match attempt {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => {
            let text = format!("{name} is held by a running frontend; stop it first");
            Err(Error::Refused(text))
        }
        Err(TryLockError::Error(err)) => Err(Error::Failed(format!("{name}: {err}"))),
    }
}
