//! The errors of the process.

use std::io;

/// Why the signer could not start or serve.
#[derive(Debug, thiserror::Error)]
pub enum SignerError {
    /// The CA directory, key or lifetime.
    #[error("ca: {0}")]
    Ca(#[from] kbf_ca::CaError),
    /// The system refused an operation.
    #[error("i/o: {0}")]
    Io(#[from] io::Error),
    /// The certificate does not parse or is not for the key.
    #[error("the root certificate does not parse or is not for the signer's key")]
    Root,
    /// The socket's directory is not a real directory of mode 0700 owned by the account.
    #[error("the socket's directory must be a directory of mode 0700 owned by this account")]
    SocketDir,
    /// Something other than a stale socket is at the socket's path.
    #[error("something other than a socket is at the socket's path")]
    SocketPath,
}
