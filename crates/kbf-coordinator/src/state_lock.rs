//! One coordinator per state file: an exclusive `flock(2)` on `<state-file>.lock`.
//!
//! The desired state is changed under in-process locks only ([`crate::desired`]), so two
//! processes on one state file would each order hosts from their own memory and replace each
//! other's writes. [`StateLock::acquire`] is the first thing a start does, before it reads any
//! file or binds any listener: a second start on the same state file is refused at once (it
//! never waits) and the refusal names the holder, as the holder wrote it into the lock file.
//!
//! The lock belongs to the open file, so the kernel frees it when the holder exits, however it
//! exits; nothing has to clean it up. The lock file itself stays: it is never removed, only
//! rewritten by the next holder. Its text is a note for the operator: a refused start reads it
//! only to show it in its refusal, and nothing parses it or decides anything from it.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// The most bytes of a lock file's note that a refusal shows.
pub const MAX_HOLDER_BYTES: u64 = 256;

/// What a refusal says when the lock file holds no note: a holder that has not written it yet.
pub const NO_HOLDER_NOTE: &str = "it left no note of who it is";

/// The lock file of `state_file`: its name with `.lock` added.
#[must_use]
pub fn path_of(state_file: &Path) -> PathBuf {
    let mut name = state_file.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// Why the state file's lock was not taken.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// Another process holds the lock: another coordinator runs on this state file.
    #[error(
        "{path}: another kbf-coordinator holds the lock of this state file ({holder}); a second \
         coordinator on one state file is refused"
    )]
    Held {
        /// The lock file.
        path: PathBuf,
        /// The holder's note in the lock file, or [`NO_HOLDER_NOTE`].
        holder: String,
    },
    /// The lock file could not be opened, locked or written.
    #[error("{path}: {source}")]
    Io {
        /// The lock file.
        path: PathBuf,
        /// Why.
        source: io::Error,
    },
}

/// The held lock of a state file; dropping it frees the lock.
#[derive(Debug)]
pub struct StateLock {
    path: PathBuf,
    /// Held for its lock.
    _file: File,
}

impl StateLock {
    /// Takes the lock of `state_file` and writes `holder` into the lock file, for the refusal
    /// of a second start to show. Refuses at once when the lock is held; it never waits. The
    /// lock file is created (owner-only) when missing; a refused start does not change it.
    ///
    /// # Errors
    /// [`LockError::Held`]: another process holds the lock. [`LockError::Io`]: the lock file
    /// cannot be opened (its directory is missing, for one), locked or written.
    pub fn acquire(state_file: &Path, holder: &str) -> Result<Self, LockError> {
        let path = path_of(state_file);
        let io_at = |source| LockError::Io {
            path: path.clone(),
            source,
        };
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .map_err(io_at)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let holder = note_of(&mut file);
                return Err(LockError::Held { path, holder });
            }
            Err(TryLockError::Error(source)) => return Err(io_at(source)),
        }
        file.set_len(0).map_err(io_at)?;
        file.write_all(format!("{holder}\n").as_bytes())
            .map_err(io_at)?;
        Ok(Self { path, _file: file })
    }

    /// The lock file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The holder's note in `file`: its first [`MAX_HOLDER_BYTES`], each character that is not
/// printable shown as `?`, trimmed; [`NO_HOLDER_NOTE`] when nothing is left. The note only
/// helps the operator, so a read that fails part way shows what was read before it.
fn note_of(file: &mut File) -> String {
    let mut bytes = Vec::new();
    // Ignored on purpose: the refusal stands whatever the note says.
    let _ = file
        .seek(SeekFrom::Start(0))
        .and_then(|_| file.take(MAX_HOLDER_BYTES).read_to_end(&mut bytes));
    let note: String = String::from_utf8_lossy(&bytes)
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect();
    let note = note.trim_matches(|c: char| c == '?' || c.is_whitespace());
    if note.is_empty() {
        NO_HOLDER_NOTE.to_owned()
    } else {
        note.to_owned()
    }
}
