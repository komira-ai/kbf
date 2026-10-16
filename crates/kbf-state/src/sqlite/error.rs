//! Why a SQLite state store could not be opened, or a backup restored.

use std::io;
use std::path::{Path, PathBuf};

use crate::StoreError;

/// Why a SQLite state store could not be opened, or a backup restored. A failure of a
/// call on an open store is a [`StoreError`] instead.
#[derive(Debug, thiserror::Error)]
pub enum SqliteError {
    /// Another process (or another store in this one) holds the lock beside the file: a
    /// second frontend started on the same state file, or `restore` run while it is in use.
    #[error(
        "state file {}: could not take {}; is another kbf process using it? ({source})",
        path.display(),
        lock.display()
    )]
    Locked {
        /// The state file.
        path: PathBuf,
        /// The lock file beside it.
        lock: PathBuf,
        /// What the lock call said.
        source: io::Error,
    },
    /// The state file is on a network filesystem, where locks and syncs are unreliable.
    #[error(
        "state file {} is on a {fs} network filesystem; keep the state store on a local disk",
        path.display()
    )]
    NetworkFs {
        /// The state file.
        path: PathBuf,
        /// The filesystem's name.
        fs: &'static str,
    },
    /// The file is not a state store this binary reads (another schema, or another
    /// application's database), or the filesystem cannot keep it in WAL mode.
    #[error("{0}")]
    NotAStateStore(String),
    /// A filesystem call failed; the text names the path.
    #[error("{0}")]
    Io(String),
    /// SQLite refused; the text names the path.
    #[error("{0}")]
    Sqlite(String),
}

impl SqliteError {
    /// The error with `path` put in front of the text of the variants that carry text.
    pub(super) fn at(self, path: &Path) -> SqliteError {
        let at = |message: String| format!("{}: {message}", path.display());
        match self {
            SqliteError::NotAStateStore(message) => SqliteError::NotAStateStore(at(message)),
            SqliteError::Io(message) => SqliteError::Io(at(message)),
            SqliteError::Sqlite(message) => SqliteError::Sqlite(at(message)),
            other => other,
        }
    }
}

impl From<io::Error> for SqliteError {
    fn from(error: io::Error) -> SqliteError {
        SqliteError::Io(error.to_string())
    }
}

impl From<rusqlite::Error> for SqliteError {
    fn from(error: rusqlite::Error) -> SqliteError {
        SqliteError::Sqlite(error.to_string())
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> StoreError {
        StoreError::Failed(error.to_string())
    }
}
