//! The socket the key is served on.

use std::convert::Infallible;
use std::fmt;
use std::fs::{self, DirBuilder};
use std::io::{self, ErrorKind};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kbf_ca::{Algorithm, Authority, FileSigner, Policy, Signer};
use x509_parser::prelude::{FromDer, X509Certificate};

use crate::error::SignerError;
use crate::validate::{Route, check_tbs};
use crate::wire;

/// How long a peer may take to send its request or take the answer.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Gives up the process's dumpable flag (`PR_SET_DUMPABLE` 0): no other process, even one
/// of the same account, can read its memory with ptrace or `/proc/<pid>/mem`, and a crash
/// writes no core file with the key in it. Call it before the key is loaded.
///
/// # Errors
/// What the system answers; on a system other than Linux, which this process refuses to
/// run on (it cannot read a peer's credentials there).
pub fn harden() -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)?;
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(io::Error::from(ErrorKind::Unsupported))
    }
}

/// The user id of the process at the other end of `stream`, as the kernel recorded it
/// when the connection was made (`SO_PEERCRED`).
#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    Ok(rustix::net::sockopt::socket_peercred(stream)?.uid.as_raw())
}

/// Without `SO_PEERCRED` no peer can be told from another: none is admitted.
#[cfg(not(target_os = "linux"))]
fn peer_uid(_stream: &UnixStream) -> io::Result<u32> {
    Err(io::Error::from(ErrorKind::Unsupported))
}

/// Makes the socket's directory (mode 0700) or accepts an existing one that is a real
/// directory of mode 0700 owned by the running account.
fn prepare_dir(dir: &Path) -> Result<(), SignerError> {
    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let meta = fs::symlink_metadata(dir)?;
    let mine = (rustix::process::geteuid().as_raw(), 0o700);
    if !meta.is_dir() || (meta.uid(), meta.mode() & 0o7777) != mine {
        return Err(SignerError::SocketDir);
    }
    Ok(())
}

/// Removes the socket at `path` unless a signer answers on it: a connection that is
/// accepted means one is serving, and a second must not take its place; only a refused
/// connection (nobody listens) marks a stale socket.
fn remove_if_stale(path: &Path) -> Result<(), SignerError> {
    match UnixStream::connect(path) {
        Ok(_) => Err(SignerError::AlreadyServing(path.to_path_buf())),
        Err(e) if e.kind() == ErrorKind::ConnectionRefused => Ok(fs::remove_file(path)?),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Binds the socket at `path`: in a directory of mode 0700 (made if missing), with mode
/// 0600. A socket left by a previous run (nobody listens on it) is replaced; one a signer
/// is serving, or anything else at `path`, is an error.
///
/// # Errors
/// [`SignerError::AlreadyServing`] for a live socket; otherwise other [`SignerError`]s.
pub fn bind(path: &Path) -> Result<UnixListener, SignerError> {
    prepare_dir(path.parent().ok_or(SignerError::SocketPath)?)?;
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => remove_if_stale(path)?,
        Ok(_) => return Err(SignerError::SocketPath),
        // Nothing there (or nothing that can be looked at, which `bind` will say).
        Err(_) => {}
    }
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// The signer's clock: Unix seconds.
type Clock = Box<dyn Fn() -> i64 + Send + Sync>;

/// The system clock in Unix seconds (0 for a clock set before 1970, which refuses all).
fn system_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

/// The CA key and the rules it signs by.
pub struct SignerServer<S: Signer> {
    clock: Clock,
    key: S,
    root_subject: Vec<u8>,
    allowed_uid: u32,
    policy: Policy,
}

impl<S: Signer> fmt::Debug for SignerServer<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SignerServer")
    }
}

impl SignerServer<FileSigner> {
    /// The server of a CA directory (`ca.key`, `ca.pem`), which must be closed to others
    /// and owned by the running account, for peers running as `allowed_uid`.
    ///
    /// # Errors
    /// [`SignerError::Ca`] for a directory, key or root that is not fit to load.
    pub fn from_dir(dir: &Path, allowed_uid: u32, policy: Policy) -> Result<Self, SignerError> {
        let authority = Authority::load_dir(dir, policy)?;
        Self::new(
            FileSigner::load(dir)?,
            authority.root_der(),
            allowed_uid,
            policy,
        )
    }
}

impl<S: Signer> SignerServer<S> {
    /// A server that signs with `key` for the CA whose self-signed root is `root_der`,
    /// for peers running as `allowed_uid`.
    ///
    /// # Errors
    /// [`SignerError::Root`] when the root does not parse or is for another key.
    pub fn new(
        key: S,
        root_der: &[u8],
        allowed_uid: u32,
        policy: Policy,
    ) -> Result<Self, SignerError> {
        let (_, root) = X509Certificate::from_der(root_der).map_err(|_| SignerError::Root)?;
        if root.public_key().raw != key.public_key() {
            return Err(SignerError::Root);
        }
        Ok(Self {
            clock: Box::new(system_now),
            key,
            root_subject: root.subject().as_raw().to_vec(),
            allowed_uid,
            policy,
        })
    }

    /// The same server reading `clock` (Unix seconds) instead of the system clock: for
    /// tests that fix time.
    #[must_use]
    pub fn with_clock(mut self, clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    /// Serves `listener`, a thread to a connection, for as long as it accepts.
    ///
    /// # Errors
    /// Why `accept` failed.
    pub fn serve(&self, listener: &UnixListener) -> io::Result<Infallible> {
        std::thread::scope(|scope| {
            loop {
                let (stream, _) = listener.accept()?;
                scope.spawn(|| self.handle(stream));
            }
        })
    }

    /// One connection. A peer that goes away mid-request is not an error of the signer.
    fn handle(&self, mut stream: UnixStream) {
        let _ = self.answer(&mut stream);
    }

    fn answer(&self, stream: &mut UnixStream) -> io::Result<()> {
        if peer_uid(stream)? != self.allowed_uid {
            return Ok(());
        }
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        let (kind, body) = wire::read_frame(stream)?;
        let (tag, payload) = self.respond(kind, &body);
        wire::write_frame(stream, tag, &payload)
    }

    fn respond(&self, kind: u8, body: &[u8]) -> (u8, Vec<u8>) {
        match kind {
            wire::PUBLIC_KEY => (wire::OK, self.key.public_key().to_vec()),
            wire::SIGN_NODE => self.sign(Route::Node, body),
            wire::SIGN_FRONTEND => self.sign(Route::Frontend, body),
            _ => (wire::FAILED, b"unknown request".to_vec()),
        }
    }

    fn sign(&self, route: Route, tbs: &[u8]) -> (u8, Vec<u8>) {
        if let Err(refusal) = check_tbs(tbs, route, self.policy, &self.root_subject, (self.clock)())
        {
            return (wire::REFUSED, refusal.to_string().into_bytes());
        }
        match self.key.sign(tbs, Algorithm::EcdsaP256Sha256) {
            Ok(signature) => (wire::OK, signature),
            Err(e) => (wire::FAILED, e.0.into_bytes()),
        }
    }
}
