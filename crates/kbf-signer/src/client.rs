//! The frontend's side of the socket: a [`Signer`] whose key is in another process.

use std::fmt::Display;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kbf_ca::{Algorithm, Custody, PublicKey, SignError, Signer};

use crate::validate::Route;
use crate::wire;

/// How long the signer may take to answer.
const IO_TIMEOUT: Duration = Duration::from_secs(10);

fn failed(e: impl Display) -> SignError {
    SignError(format!("signer socket: {e}"))
}

/// One request and its answer.
fn round_trip(path: &Path, kind: u8, body: &[u8]) -> Result<Vec<u8>, SignError> {
    let mut stream = UnixStream::connect(path).map_err(failed)?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).map_err(failed)?;
    stream.set_write_timeout(Some(IO_TIMEOUT)).map_err(failed)?;
    wire::write_frame(&mut stream, kind, body).map_err(failed)?;
    let (tag, payload) = wire::read_frame(&mut stream).map_err(failed)?;
    let text = String::from_utf8_lossy(&payload);
    match tag {
        wire::OK => Ok(payload),
        wire::REFUSED => Err(SignError(format!("the signer refused: {text}"))),
        _ => Err(SignError(format!("the signer failed: {text}"))),
    }
}

/// The CA key behind a `kbf-signer` socket. Every [`Signer::sign`] is one connection; the
/// route says which door the to-be-signed structures go through.
#[derive(Debug)]
pub struct SocketSigner {
    path: PathBuf,
    route: Route,
    spki: Vec<u8>,
}

impl SocketSigner {
    /// Connects, asks for the public key and keeps it.
    ///
    /// # Errors
    /// [`SignError`] when the socket cannot be reached or refuses this account, or what
    /// it sends back is not an ECDSA P-256 key.
    pub fn connect(path: impl Into<PathBuf>, route: Route) -> Result<Self, SignError> {
        let path = path.into();
        let spki = round_trip(&path, wire::PUBLIC_KEY, &[])?;
        PublicKey::from_spki(&spki).map_err(failed)?;
        Ok(Self { path, route, spki })
    }
}

impl Signer for SocketSigner {
    fn public_key(&self) -> &[u8] {
        &self.spki
    }

    fn custody(&self) -> Custody {
        Custody::External
    }

    fn sign(&self, tbs: &[u8], algorithm: Algorithm) -> Result<Vec<u8>, SignError> {
        let Algorithm::EcdsaP256Sha256 = algorithm;
        let kind = match self.route {
            Route::Node => wire::SIGN_NODE,
            Route::Frontend => wire::SIGN_FRONTEND,
        };
        round_trip(&self.path, kind, tbs)
    }
}
