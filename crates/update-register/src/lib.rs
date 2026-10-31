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
//! [`register`] blocks until the updater has answered. The updater answers as soon as it has
//! read the line, which this client sends at once.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::SocketAddr;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

/// The longest registration line the updater reads, newline included.
pub const MAX_LINE: usize = 4096;

/// How long `register` waits for the answer.
pub const ANSWER_DEADLINE_SECS: u64 = 10;

/// The most of an answer that is read; an answer is one short line.
const MAX_ANSWER: u64 = 512;

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

fn register_on(
    stream: UnixStream,
    name: &str,
    version: &str,
    health: Option<&Health>,
    _deadline: Duration,
) -> Result<Registration, Error> {
    let line = encode(name, version, health);
    if line.len() > MAX_LINE {
        return Err(Error::TooLong { len: line.len() });
    }
    // An updater that is full answers and closes without reading the line, so the write can
    // fail while the answer is waiting: the answer is read first and says why.
    let sent = (&stream).write_all(&line);
    match (answer(&stream), sent) {
        (Ok(()), _) => Ok(Registration {
            _connection: stream,
        }),
        (Err(Error::NoAnswer), Err(e)) => Err(Error::Io(e)),
        (Err(e), _) => Err(e),
    }
}

/// The registration line: one JSON object and a newline.
fn encode(name: &str, version: &str, health: Option<&Health>) -> Vec<u8> {
    let health = health.map(|h| json!({ "addr": h.addr.to_string(), "path": h.path }));
    let mut line = json!({ "name": name, "version": version, "health": health }).to_string();
    line.push('\n');
    line.into_bytes()
}

/// Reads the updater's answer: `{"ok":true,"error":null}` or `{"ok":false,"error":"<token>"}`.
fn answer(stream: &UnixStream) -> Result<(), Error> {
    let mut text = String::new();
    BufReader::new(stream.take(MAX_ANSWER)).read_line(&mut text)?;
    if text.is_empty() {
        return Err(Error::NoAnswer);
    }
    let reply: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    match (reply["ok"].as_bool(), reply["error"].as_str()) {
        (Some(true), None) => Ok(()),
        (Some(false), Some(token)) => Err(Refusal::from_token(token).into()),
        _ => Err(Error::BadAnswer(text)),
    }
}

#[cfg(test)]
mod tests;
