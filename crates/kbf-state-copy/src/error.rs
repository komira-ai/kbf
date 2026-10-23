//! What stops a run, and the exit status each kind has.

use std::io;

use kbf_state::postgres::ConfigError;

/// Why a run did not finish.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A precondition does not hold. Nothing was changed. Exit status 2.
    #[error("refused: {0}")]
    Refused(String),
    /// A call failed, or the copy did not match the state file. Exit status 1.
    #[error("failed: {0}")]
    Failed(String),
}

impl Error {
    /// The exit status of a process that ends with this error.
    #[must_use]
    pub fn code(&self) -> u8 {
        match self {
            Error::Refused(_) => 2,
            Error::Failed(_) => 1,
        }
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Error {
        Error::Failed(err.to_string())
    }
}

impl From<rusqlite::Error> for Error {
    fn from(err: rusqlite::Error) -> Error {
        Error::Failed(format!("the state file: {err}"))
    }
}

impl From<ConfigError> for Error {
    fn from(err: ConfigError) -> Error {
        Error::Failed(err.to_string())
    }
}

impl From<postgres::Error> for Error {
    fn from(err: postgres::Error) -> Error {
        Error::Failed(format!("the target: {}", describe(&err)))
    }
}

/// What the server said, which the client's own text leaves out for a refusal ("db error").
fn describe(err: &postgres::Error) -> String {
    let Some(db) = err.as_db_error() else {
        return err.to_string();
    };
    let code = db.code().code();
    format!("{}: {} (SQLSTATE {code})", db.severity(), db.message())
}
