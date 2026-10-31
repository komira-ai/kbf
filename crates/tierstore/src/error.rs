//! The errors of a [`Backend`](crate::Backend) call.

/// Why a backend call failed. Cloned, so one failure can be reported to every caller that
/// waited on it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// No object has this key.
    #[error("object {0} not found")]
    NotFound(String),
    /// A create-only put on a key that exists.
    #[error("object {0} already exists")]
    AlreadyExists(String),
    /// A put that replaces an object only if it has a given content tag, and the object has
    /// another.
    #[error("object {0} does not have the expected content tag")]
    PreconditionFailed(String),
    /// The backend has no room for the object. Freeing room may let a retry succeed.
    #[error("backend is full: {0}")]
    Full(String),
    /// A client write to a key under a prefix the proxy keeps for itself.
    #[error("key {0} is under a reserved prefix")]
    Reserved(String),
    /// A body whose bytes do not hash to the name of the object it is written under (a key
    /// named by its content). Nothing is stored.
    #[error("the body of {0} does not hash to the name of the object")]
    BadDigest(String),
    /// A completion of a multipart upload names a part that was not uploaded, or one whose
    /// content tag is not the one that was named.
    #[error("invalid part: {0}")]
    InvalidPart(String),
    /// The backend does not do this call: the multipart calls of a [`Backend`](crate::Backend)
    /// that does not override them.
    #[error("not supported: {0}")]
    Unsupported(String),
    /// The backend did not answer (down, timed out, refused the connection). Retrying later
    /// may succeed.
    #[error("backend unavailable: {0}")]
    Unavailable(String),
    /// Any other failure, such as a range that starts past the end of the object.
    #[error("backend error: {0}")]
    Other(String),
}

impl Error {
    /// Whether the backend as a whole is out of reach, so that trying the next object of a
    /// batch would fail the same way.
    #[must_use]
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Error::Unavailable(_))
    }
}
