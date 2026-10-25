//! Why the tool ended without finishing, and the exit status that says so.

use std::io;

use kbf_pack::AdoptError;

/// How a run ends when it does not finish.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The command line, a file it names or the checkpoint is not acceptable. Nothing was
    /// changed.
    #[error("refused: {0}")]
    Refused(String),
    /// A call failed. The index may hold the records of the segments handled before the
    /// failure, and a checkpoint says which.
    #[error("failed: {0}")]
    Failed(String),
}

impl Error {
    /// A refusal.
    pub fn refused(why: impl Into<String>) -> Self {
        Self::Refused(why.into())
    }

    /// The exit status: 2 for a refusal, 1 for a failure.
    #[must_use]
    pub fn code(&self) -> u8 {
        match self {
            Self::Refused(_) => 2,
            Self::Failed(_) => 1,
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Failed(error.to_string())
    }
}

impl From<AdoptError> for Error {
    /// A checkpoint that is not this run's is a refusal, as it is found before anything is
    /// read; every other end of a run is a failure.
    fn from(error: AdoptError) -> Self {
        match error {
            AdoptError::ResumeMismatch | AdoptError::ResumeKey(_) => {
                Self::Refused(error.to_string())
            }
            other => Self::Failed(other.to_string()),
        }
    }
}
