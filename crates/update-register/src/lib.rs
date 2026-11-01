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
//! [`ANSWER_DEADLINE_SECS`] have passed, so that a dead or hung updater cannot hold a service from
//! starting. The deadline is one, for the whole call: it runs from before the connect, and the
//! connect (to a listener that is not accepting and whose backlog is full, which would otherwise
//! wait for ever), the write of the line and each read of the answer wait only for what is left
//! of it. A service that registers on its start path therefore waits at most the deadline (10 s)
//! when the updater hangs; one that cannot wait that long registers from a thread of its own.

use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The longest registration line the updater reads, newline included.
pub const MAX_LINE: usize = 4096;

/// How long `register` waits, in all, from before it connects until it has the updater's answer:
/// the updater's own limit on a line, so an updater that answers at all does so within it.
pub const ANSWER_DEADLINE_SECS: u64 = 10;

/// The most of an answer that is read; an answer is one short line.
const MAX_ANSWER: usize = 512;

/// Where the client reads the time. `register` reads `Instant::now`; the tests read a clock they
/// move, so that the deadline is passed by the test and not by waiting.
type Clock<'a> = &'a dyn Fn() -> Instant;

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
    /// The updater closed the connection, or the deadline passed (connecting, writing the line or
    /// waiting for the answer), without an answer.
    #[error("the updater closed the connection or let the deadline pass without answering")]
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
    register_within(
        socket,
        name,
        version,
        health,
        Duration::from_secs(ANSWER_DEADLINE_SECS),
        &Instant::now,
    )
}

/// `register` with `deadline` for the whole call, the connect included, on the clock `now`.
fn register_within(
    socket: &Path,
    name: &str,
    version: &str,
    health: Option<&Health>,
    deadline: Duration,
    now: Clock<'_>,
) -> Result<Registration, Error> {
    let end = now() + deadline;
    // A connect that waited out the deadline (a full backlog), or had none left, is the updater
    // not answering; any other failure (no socket, nobody listening, no permission) is the path
    // not being reachable.
    let stream = connect(socket, end, now).map_err(|source| {
        if timed_out(&source) {
            Error::NoAnswer
        } else {
            Error::Connect {
                path: socket.to_path_buf(),
                source,
            }
        }
    })?;
    register_on(stream, name, version, health, end, now)
}

/// Connects to `path` before `end` on the clock `now`, waiting only for what is left for a
/// listener whose backlog is full.
///
/// `std` has no connect timeout, and Linux makes a unix socket's connect to a full backlog wait
/// for as long as the socket's send timeout says, for ever by default. So the socket is made here
/// and given what is left as its send timeout before each try: the connect then fails with
/// `WouldBlock` once that has passed. The socket is owned from the moment it exists, so an error
/// drops it and no descriptor is left; no thread is started. (The BSD-derived kernels refuse a
/// connection to a full backlog rather than wait, which is `Error::Connect`.)
fn connect(path: &Path, end: Instant, now: Clock<'_>) -> io::Result<UnixStream> {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};

    // The checks of `std`'s own connect: no NUL in the path, and room for the one that ends it.
    std::os::unix::net::SocketAddr::from_pathname(path)?;
    let addr = SocketAddrUnix::new(path)?;
    let stream = UnixStream::from(rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )?);
    with_what_is_left(end, now, |wait| {
        stream.set_write_timeout(Some(wait))?;
        rustix::net::connect(&stream, &addr).map_err(io::Error::from)
    })?;
    Ok(stream)
}

/// Calls `op` with the time left until `end` on the clock `now`, and again, with what is left
/// then, while a signal interrupts it. With none left `op` is not called and the result is
/// `TimedOut`; so a peer, or signals, that keep interrupting cannot carry a call past `end`.
fn with_what_is_left<T>(
    end: Instant,
    now: Clock<'_>,
    mut op: impl FnMut(Duration) -> io::Result<T>,
) -> io::Result<T> {
    loop {
        let wait = remaining(end, now).ok_or(io::ErrorKind::TimedOut)?;
        match op(wait) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            done => return done,
        }
    }
}

/// `register` over a connection made, until `end` on the clock `now`, for the write and for the
/// whole answer.
fn register_on(
    stream: UnixStream,
    name: &str,
    version: &str,
    health: Option<&Health>,
    end: Instant,
    now: Clock<'_>,
) -> Result<Registration, Error> {
    let line = encode(name, version, health);
    if line.len() > MAX_LINE {
        return Err(Error::TooLong { len: line.len() });
    }
    // An updater that is full answers and closes without reading the line, so the write can
    // fail while the answer is waiting: the answer is read first and says why.
    let sent = send(&line, end, now, |wait, bytes| {
        stream.set_write_timeout(Some(wait))?;
        (&stream).write(bytes)
    });
    match (answer(&stream, end, now), sent) {
        (Ok(()), Ok(())) => Ok(Registration {
            _connection: stream,
        }),
        // The deadline passed while the line was going: that is the updater not answering, as
        // when it passes during the connect or the answer.
        (Err(Error::NoAnswer), Err(e)) if timed_out(&e) => Err(Error::NoAnswer),
        (Ok(()), Err(e)) | (Err(Error::NoAnswer), Err(e)) => Err(Error::Io(e)),
        (Err(e), _) => Err(e),
    }
}

/// Writes `line` before `end` with `write`, which is given the time left and writes what it can
/// of the bytes in that time. The clock is read before every write, the one after a write that
/// went in part and the one after a signal included, and each waits only for what is left: a
/// deadline that has passed is `TimedOut`, and a write that waited out what was left is
/// `WouldBlock`.
fn send(
    line: &[u8],
    end: Instant,
    now: Clock<'_>,
    mut write: impl FnMut(Duration, &[u8]) -> io::Result<usize>,
) -> io::Result<()> {
    let mut rest = line;
    while !rest.is_empty() {
        let n = with_what_is_left(end, now, |wait| write(wait, rest))?;
        rest = &rest[n..];
    }
    Ok(())
}

/// The time left until `end` on the clock `now`, or `None` once there is none: a zero timeout
/// would be refused by `std` (and read as no timeout at all by the system call).
fn remaining(end: Instant, now: Clock<'_>) -> Option<Duration> {
    end.checked_duration_since(now())
        .filter(|left| !left.is_zero())
}

/// Whether an I/O error is a wait that ran out: a socket timeout reports `WouldBlock` (or
/// `TimedOut`), and `with_what_is_left` reports a deadline already passed as `TimedOut`.
fn timed_out(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// The registration line: one JSON object and a newline.
fn encode(name: &str, version: &str, health: Option<&Health>) -> Vec<u8> {
    let health = health.map(|h| json!({ "addr": h.addr.to_string(), "path": h.path }));
    let mut line = json!({ "name": name, "version": version, "health": health }).to_string();
    line.push('\n');
    line.into_bytes()
}

/// Reads the updater's answer, one line, until `end` on the clock `now`: `{"ok":true,"error":null}`
/// or `{"ok":false,"error":"<token>"}`.
///
/// Nothing at all is [`Error::NoAnswer`], and so is the deadline passing. Something that is not
/// one whole line the client understands is [`Error::BadAnswer`], a line cut short before its
/// newline included: the updater writes the newline with the rest, so a success that lacks it was
/// cut by a connection that died, and is not believed. The clock is read before each read, which
/// waits only for what is left, and a read with nothing left is not made: a peer that sends the
/// answer a byte at a time cannot hold the client past the deadline. A read a signal interrupted
/// is made again, with what is left then.
fn answer(stream: &UnixStream, end: Instant, now: Clock<'_>) -> Result<(), Error> {
    let mut text = Vec::new();
    let mut chunk = [0u8; 128];
    while !text.contains(&b'\n') && text.len() < MAX_ANSWER {
        let read = with_what_is_left(end, now, |wait| {
            stream.set_read_timeout(Some(wait))?;
            (&*stream).read(&mut chunk)
        });
        match read {
            Ok(0) => break,
            Ok(n) => text.extend_from_slice(&chunk[..n]),
            Err(e) if timed_out(&e) => return Err(Error::NoAnswer),
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
