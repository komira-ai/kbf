//! The CA at the frontend's first start (`docs/design/install-and-enrollment.md`, sections
//! 6.1 and 6.2), and the frontend's side of its signer.
//!
//! `--ca-dir` is the CA directory of `kbf-ca`: `ca.key` (mode 0600) and `ca.pem`. What a
//! start does with it depends on where the key lives:
//!
//! - **Production: `--ca-signer=<socket>`.** The key belongs to a `kbf-signer` process
//!   running as another account; this process holds none. `--ca-dir` must already hold
//!   `ca.pem`, the public root, which the signer made when it started (`kbf_signer::ensure`)
//!   or the operator placed there with its key. The root is imported against the signer's
//!   public key and refused unless the two agree, so a replaced `ca.pem` cannot change
//!   what the CA is. Two authorities are built, one per route of the signer (the node
//!   route cannot mint a frontend certificate, the frontend route can mint nothing else),
//!   and each is checked to be external ([`kbf_ca::Authority::require_external`]).
//! - **Development and tests: `--ca-dev-file-signer`.** The key is a file in this
//!   process. `kbf_signer::ensure` makes the directory when it is empty (a new key and
//!   root, written once) and loads it when it holds `ca.key` and `ca.pem`; it never
//!   overwrites a CA, and a directory holding anything else is an error. Import means an
//!   operator placed both files in the directory before the first start.
//!
//! There is no way to select the file signer except its flag, so a production start (which
//! names a socket, or fails) cannot end up holding the key in the frontend's memory.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kbf_ca::{Authority, Bundle, CaError, Entropy, Policy, SignError};
use kbf_signer::{Route, SignerError, SocketSigner, Started, ensure};
use kbf_types::CaPin;

use super::{PinError, PinSource};

/// Where the CA key lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignerChoice {
    /// In the `kbf-signer` process serving this socket.
    External(PathBuf),
    /// In a file in this process: development and tests only.
    DevFile,
}

/// Why the CA could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum CaOpenError {
    /// The directory, the key or the root is not fit to use.
    #[error("--ca-dir: {0}")]
    Ca(#[from] CaError),
    /// The directory is not in a state `kbf_signer::ensure` accepts.
    #[error("--ca-dir: {0}")]
    Dir(#[from] SignerError),
    /// The signer socket could not be used.
    #[error("--ca-signer: {0}")]
    Signer(#[from] SignError),
    /// With an external signer the directory must hold the root, which the signer
    /// makes; it does not hold it.
    #[error(
        "--ca-dir {dir}: no ca.pem here. With --ca-signer the root is made by the \
         kbf-signer process (give it this directory, readable by this account, or copy its \
         ca.pem here), not by the frontend, which holds no CA key"
    )]
    NoRoot {
        /// The directory.
        dir: PathBuf,
    },
    /// The root could not be read.
    #[error("--ca-dir {dir}: {source}")]
    Read {
        /// The directory.
        dir: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
}

/// The CA a frontend issues from: one authority for the node route and one for the
/// frontend route. With a file signer they are the same value.
pub struct Ca {
    node: Arc<Authority>,
    frontend: Arc<Authority>,
    started: Started,
}

impl fmt::Debug for Ca {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ca")
            .field("started", &self.started)
            .finish_non_exhaustive()
    }
}

impl Ca {
    /// Opens the CA of `dir` as `signer` says, at unix time `now` (the time a new root
    /// would be made at).
    ///
    /// # Errors
    /// [`CaOpenError`]; nothing in `dir` is overwritten.
    pub fn open(
        dir: &Path,
        signer: &SignerChoice,
        policy: Policy,
        now: i64,
        entropy: &mut dyn Entropy,
    ) -> Result<Self, CaOpenError> {
        match signer {
            SignerChoice::DevFile => {
                let (authority, started) = ensure(dir, policy, now, entropy)?;
                let authority = Arc::new(authority);
                Ok(Self {
                    node: Arc::clone(&authority),
                    frontend: authority,
                    started,
                })
            }
            SignerChoice::External(socket) => {
                let root = read_root(dir)?;
                let through = |route| -> Result<Arc<Authority>, CaOpenError> {
                    let signer = SocketSigner::connect(socket, route)?;
                    let authority = Authority::import(Arc::new(signer), policy, &root)?;
                    Ok(Arc::new(authority.require_external()?))
                };
                Ok(Self {
                    node: through(Route::Node)?,
                    frontend: through(Route::Frontend)?,
                    started: Started::Loaded,
                })
            }
        }
    }

    /// The authority that issues node certificates.
    #[must_use]
    pub fn node(&self) -> &Authority {
        &self.node
    }

    /// The authority that issues the frontend's own certificate.
    #[must_use]
    pub fn frontend(&self) -> &Authority {
        &self.frontend
    }

    /// Whether this start made the CA or loaded one.
    #[must_use]
    pub const fn started(&self) -> Started {
        self.started
    }

    /// The pin of the root: what a token carries.
    #[must_use]
    pub fn pin(&self) -> CaPin {
        self.node.pin()
    }

    /// The CA certificates a node trusts, as PEM, current root first.
    #[must_use]
    pub fn bundle_pem(&self) -> String {
        self.node.bundle().to_pem()
    }
}

/// The root of `dir/ca.pem`.
fn read_root(dir: &Path) -> Result<Vec<u8>, CaOpenError> {
    let path = dir.join(Authority::CERT_FILE);
    let pem = match std::fs::read(&path) {
        Ok(pem) => pem,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(CaOpenError::NoRoot {
                dir: dir.to_owned(),
            });
        }
        Err(source) => {
            return Err(CaOpenError::Read {
                dir: dir.to_owned(),
                source,
            });
        }
    };
    Ok(Bundle::from_pem(&pem)?.certs()[0].clone())
}

/// The pin of a token string, from the CA this frontend loaded.
#[derive(Clone, Debug)]
pub struct CaPins(pub Arc<Ca>);

impl PinSource for CaPins {
    fn pin(&self) -> Result<CaPin, PinError> {
        Ok(self.0.pin())
    }
}

#[cfg(test)]
mod tests;
