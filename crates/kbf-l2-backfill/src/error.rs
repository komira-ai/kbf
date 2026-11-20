//! Why the tool ended without finishing, and the exit status that says so.

use std::io;

/// How a run ends when it does not finish.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The command line or a file it names is not acceptable. Nothing was changed.
    #[error("refused: {0}")]
    Refused(String),
    /// A call failed, or trees failed. The sidecars written before the failure stay written.
    #[error("failed: {0}")]
    Failed(String),
}

impl Error {
    /// A refusal.
    pub fn refused(why: impl Into<String>) -> Self {
        Self::Refused(why.into())
    }

    /// A failure.
    pub fn failed(why: impl Into<String>) -> Self {
        Self::Failed(why.into())
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
