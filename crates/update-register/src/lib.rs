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
    let wait = remaining(end, now).ok_or(Error::NoAnswer)?;
    // A connect that waited out the deadline (a full backlog) is the updater not answering; any
    // other failure (no socket, nobody listening, no permission) is the path not being reachable.
    let stream = connect(socket, wait).map_err(|source| {
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

/// Connects to `path`, waiting at most `wait` for a listener whose backlog is full.
///
/// `std` has no connect timeout, and Linux makes a unix socket's connect to a full backlog wait
/// for as long as the socket's send timeout says, for ever by default. So the socket is made here
/// and given `wait` as its send timeout before it connects: the connect then fails with
/// `WouldBlock` once `wait` has passed. The socket is owned from the moment it exists, so an
/// error drops it and no descriptor is left; no thread is started.
#[cfg(target_os = "linux")]
fn connect(path: &Path, wait: Duration) -> io::Result<UnixStream> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    // The checks of `std`'s own connect: no NUL in the path, and room for the one that ends it.
    std::os::unix::net::SocketAddr::from_pathname(path)?;
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: `sockaddr_un` is plain data, for which all zeroes is a valid value.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (to, from) in addr.sun_path.iter_mut().zip(bytes) {
        *to = *from as libc::c_char;
    }
    let len = std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
    // SAFETY: a plain system call with constant arguments.
    let fd =
        cvt(unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) })?;
    // SAFETY: `fd` was just returned by `socket` and nothing else owns it.
    let stream = UnixStream::from(unsafe { OwnedFd::from_raw_fd(fd) });
    stream.set_write_timeout(Some(wait))?;
    // SAFETY: `addr` is a `sockaddr_un` that lives across the call, and `len` (at most its size,
    // which `from_pathname` checked) is the length of the address in it, terminating NUL included.
    cvt(unsafe {
        libc::connect(
            stream.as_raw_fd(),
            (&raw const addr).cast::<libc::sockaddr>(),
            len as libc::socklen_t,
        )
    })?;
    Ok(stream)
}

/// Elsewhere `std`'s connect: the BSD-derived kernels refuse a connection to a full backlog
/// rather than wait for room in it.
#[cfg(not(target_os = "linux"))]
fn connect(path: &Path, _wait: Duration) -> io::Result<UnixStream> {
    UnixStream::connect(path)
}

/// A system call's result: -1 is the error in `errno`.
#[cfg(target_os = "linux")]
fn cvt(result: libc::c_int) -> io::Result<libc::c_int> {
    match result {
        -1 => Err(io::Error::last_os_error()),
        fd => Ok(fd),
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
    let sent = send(&stream, &line, end, now);
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

/// Writes `line` before `end`, waiting only for what is left: the timeout is set from the clock
/// once, and the line, at most `MAX_LINE` bytes, goes in one write unless that wait runs out
/// (`write_all` retries a write a signal interrupted). A deadline that has passed is `TimedOut`, and
/// a write that waited out what was left is `WouldBlock`.
fn send(stream: &UnixStream, line: &[u8], end: Instant, now: Clock<'_>) -> io::Result<()> {
    let wait = remaining(end, now).ok_or(io::ErrorKind::TimedOut)?;
    stream.set_write_timeout(Some(wait))?;
    (&*stream).write_all(line)
}

/// The time left until `end` on the clock `now`, or `None` once there is none: a zero timeout
/// would be refused by `std` (and read as no timeout at all by the system call).
fn remaining(end: Instant, now: Clock<'_>) -> Option<Duration> {
    end.checked_duration_since(now())
        .filter(|left| !left.is_zero())
}

/// Whether an I/O error is a wait that ran out: a socket timeout reports `WouldBlock` (or
/// `TimedOut`), and `send` reports a deadline already passed as `TimedOut`.
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
/// answer a byte at a time cannot hold the client past the deadline.
fn answer(stream: &UnixStream, end: Instant, now: Clock<'_>) -> Result<(), Error> {
    let mut text = Vec::new();
    let mut chunk = [0u8; 128];
    while !text.contains(&b'\n') && text.len() < MAX_ANSWER {
        let wait = remaining(end, now).ok_or(Error::NoAnswer)?;
        stream.set_read_timeout(Some(wait))?;
        match (&*stream).read(&mut chunk) {
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
