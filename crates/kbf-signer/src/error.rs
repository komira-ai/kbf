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
    /// The CA directory is neither empty nor exactly `ca.key` and `ca.pem`.
    #[error("{}", ca_dir_message(.dir, .found))]
    CaDirState {
        /// The directory.
        dir: std::path::PathBuf,
        /// What is in it, sorted.
        found: Vec<String>,
    },
    /// A signer answers on the socket's path.
    #[error("a signer is already serving {}", .0.display())]
    AlreadyServing(std::path::PathBuf),
    /// Something other than a stale socket is at the socket's path.
    #[error("something other than a socket is at the socket's path")]
    SocketPath,
}

/// What is wrong with a CA directory and what to do about it.
fn ca_dir_message(dir: &std::path::Path, found: &[String]) -> String {
    let dir = dir.display();
    let what = match found {
        [only] if only == "ca.key" => {
            "has only ca.key: remove it if no certificate was ever issued, or restore ca.pem"
                .to_owned()
        }
        [only] if only == "ca.pem" => {
            "has only ca.pem: restore ca.key (the root signs nothing without it), or remove \
             ca.pem to make a new CA"
                .to_owned()
        }
        _ => format!(
            "holds {}: it must be empty or exactly ca.key and ca.pem; move the other files \
             out",
            found.join(", ")
        ),
    };
    format!("the CA directory {dir} {what}")
}
