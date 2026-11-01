//! The client of the registration socket of a host updater: a service says it is alive, which
//! version it runs and, if it has a listener, where its health answers
//! (docs/design/host-updates.md, 2.1).
//!
//! ```no_run
//! # use std::path::Path;
//! let _registration = update_register::register(
//!     Path::new("/run/updater/register.sock"),
//!     "tierstore",
//!     "1.4.0",
//!     Some(&update_register::Health::new("127.0.0.1:9301".parse().unwrap(), "/readyz")),
//! )?;
//! // ... run the service; dropping `_registration` tells the updater it stopped.
//! # Ok::<(), update_register::Error>(())
//! ```
//!
//! The message carries liveness and version and nothing else: the updater reads who sent it
//! from the kernel (`SO_PEERCRED`) and compares that user with the one the service's declaration
//! names, so a registration from another user is refused. The connection is the liveness: the
//! [`Registration`] holds it open, and when it is dropped or the process ends the updater reports
//! the service stopped. A service that restarts registers again.
//!
//! [`register`] blocks until the updater has answered, and gives up with [`Error::NoAnswer`] when
//! [`ANSWER_DEADLINE_SECS`] have passed, so that a stalled updater cannot hold a service from
//! starting. Connecting has no bound: a unix socket whose listener is not accepting and whose
//! backlog is full blocks `connect`, and `std` has no connect timeout; a caller that cannot
//! wait registers from a thread of its own.

use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The longest registration line the updater reads, newline included.
pub const MAX_LINE: usize = 4096;

/// How long `register` waits for the updater's answer, in all: the updater's own limit on a
/// line, so an updater that answers at all does so within it.
pub const ANSWER_DEADLINE_SECS: u64 = 10;

/// The most of an answer that is read; an answer is one short line.
const MAX_ANSWER: usize = 512;

/// The smallest timeout a socket takes: a zero one is refused (`InvalidInput`) by `std`, and the
/// system call reads it as no timeout at all. A deadline of zero, or one that has passed, is this.
const SMALLEST_TIMEOUT: Duration = Duration::from_micros(1);

/// Where a service's health answers: the updater issues a `GET` of `path` on `addr` and reads
/// a 2xx as ready. The address must be loopback and its port one the declaration grants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Health {
    pub addr: SocketAddr,
    /// The request path, starting with `/` (`/readyz`).
    pub path: String,
}

impl Health {
    pub fn new(addr: SocketAddr, path: impl Into<String>) -> Health {
        Health {
            addr,
            path: path.into(),
        }
    }
}

/// Why the updater refused a registration; the reason it sent is the `token`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("the updater did not read the registration as one JSON object within its size limit")]
    Malformed,
    #[error("no declaration names that service")]
    UnknownService,
    #[error("the declaration names another user for that service")]
    WrongUser,
    #[error("the health address is not a loopback address with a path")]
    HealthAddress,
    #[error("the declaration does not grant that health port")]
    HealthPort,
    #[error("the updater holds as many connections as it allows")]
    Busy,
    #[error("the updater could not keep the registration")]
    Internal,
    /// A reason a newer updater may send.
    #[error("the updater refused the registration: {0}")]
    Other(String),
}

impl Refusal {
    /// The reason as the updater writes it.
    pub fn token(&self) -> &str {
        match self {
            Refusal::Malformed => "malformed",
            Refusal::UnknownService => "unknown_service",
            Refusal::WrongUser => "wrong_user",
            Refusal::HealthAddress => "health_address",
            Refusal::HealthPort => "health_port",
            Refusal::Busy => "busy",
            Refusal::Internal => "internal",
            Refusal::Other(token) => token,
        }
    }

    fn from_token(token: &str) -> Refusal {
        match token {
            "malformed" => Refusal::Malformed,
            "unknown_service" => Refusal::UnknownService,
            "wrong_user" => Refusal::WrongUser,
            "health_address" => Refusal::HealthAddress,
            "health_port" => Refusal::HealthPort,
            "busy" => Refusal::Busy,
            "internal" => Refusal::Internal,
            other => Refusal::Other(other.to_owned()),
        }
    }
}

/// Why a registration did not happen.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the registration socket {path:?} is not reachable: {source}")]
    Connect { path: PathBuf, source: io::Error },
    #[error("the registration is {len} bytes, over the {MAX_LINE} the updater reads")]
    TooLong { len: usize },
    #[error("talking to the updater failed: {0}")]
    Io(#[from] io::Error),
    #[error("the updater closed the connection without answering")]
    NoAnswer,
    #[error("the updater's answer is not understood: {0:?}")]
    BadAnswer(String),
    #[error(transparent)]
    Refused(#[from] Refusal),
}

/// A registration the updater holds. Dropping it closes the connection, which the updater reads
/// as the service having stopped.
#[must_use = "dropping the registration tells the updater the service stopped"]
#[derive(Debug)]
pub struct Registration {
    _connection: UnixStream,
}

/// Registers `name`, running `version`, with the updater listening on `socket`.
///
/// `health` is `None` for a service with no listener; the updater then reports it healthy while
/// registered and leaves its health to the hook.
pub fn register(
    socket: &Path,
    name: &str,
    version: &str,
    health: Option<&Health>,
) -> Result<Registration, Error> {
    let stream = UnixStream::connect(socket).map_err(|source| Error::Connect {
        path: socket.to_path_buf(),
        source,
    })?;
    register_on(
        stream,
        name,
        version,
        health,
        Duration::from_secs(ANSWER_DEADLINE_SECS),
    )
}

/// `register` over a connection made, with `deadline` for the whole answer (and for the write).
fn register_on(
    stream: UnixStream,
    name: &str,
    version: &str,
    health: Option<&Health>,
    deadline: Duration,
) -> Result<Registration, Error> {
    let line = encode(name, version, health);
    if line.len() > MAX_LINE {
        return Err(Error::TooLong { len: line.len() });
    }
    let end = Instant::now() + deadline;
    stream.set_write_timeout(Some(left(end)))?;
    // An updater that is full answers and closes without reading the line, so the write can
    // fail while the answer is waiting: the answer is read first and says why.
    let sent = (&stream).write_all(&line);
    match (answer(&stream, end), sent) {
        (Ok(()), Ok(())) => Ok(Registration {
            _connection: stream,
        }),
        (Ok(()), Err(e)) | (Err(Error::NoAnswer), Err(e)) => Err(Error::Io(e)),
        (Err(e), _) => Err(e),
    }
}

/// The time left until `end`, and never zero.
fn left(end: Instant) -> Duration {
    end.saturating_duration_since(Instant::now())
        .max(SMALLEST_TIMEOUT)
}

/// The registration line: one JSON object and a newline.
fn encode(name: &str, version: &str, health: Option<&Health>) -> Vec<u8> {
    let health = health.map(|h| json!({ "addr": h.addr.to_string(), "path": h.path }));
    let mut line = json!({ "name": name, "version": version, "health": health }).to_string();
    line.push('\n');
    line.into_bytes()
}

/// Reads the updater's answer, one line, until `end`: `{"ok":true,"error":null}` or
/// `{"ok":false,"error":"<token>"}`.
///
/// Nothing at all is [`Error::NoAnswer`], and so is the deadline passing. Something that is not
/// one whole line the client understands is [`Error::BadAnswer`], a line cut short before its
/// newline included: the updater writes the newline with the rest, so a success that lacks it was
/// cut by a connection that died, and is not believed. Each read waits only for what is left of
/// the deadline, and an answer is read to at most `MAX_ANSWER` bytes, so the deadline holds for
/// the whole answer to within one short read.
fn answer(stream: &UnixStream, end: Instant) -> Result<(), Error> {
    let mut text = Vec::new();
    let mut chunk = [0u8; 128];
    while !text.contains(&b'\n') && text.len() < MAX_ANSWER {
        stream.set_read_timeout(Some(left(end)))?;
        match (&*stream).read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => text.extend_from_slice(&chunk[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Err(Error::NoAnswer);
            }
            Err(e) => return Err(e.into()),
        }
    }
    if text.is_empty() {
        return Err(Error::NoAnswer);
    }
    let whole = text.contains(&b'\n');
    let reply: Value = serde_json::from_slice(&text).unwrap_or(Value::Null);
    match (whole, reply["ok"].as_bool(), reply["error"].as_str()) {
        (true, Some(true), None) => Ok(()),
        (true, Some(false), Some(token)) => Err(Refusal::from_token(token).into()),
        _ => Err(Error::BadAnswer(
            String::from_utf8_lossy(&text).into_owned(),
        )),
    }
}

#[cfg(test)]
mod tests;
