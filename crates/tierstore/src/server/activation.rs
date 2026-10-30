//! Listening sockets that come from the service manager (systemd socket activation) instead of
//! being bound by the process.
//!
//! With `--listen-from-systemd` (`server.listen_from_systemd` in a file) the process does not bind
//! `--listen` and `--oci-listen`: it takes the sockets the manager bound and holds. A socket
//! that the manager holds is not closed when the process exits, so the connections that arrive
//! between one process and the next wait in the kernel's queue of that socket and are accepted
//! by the next, and are not refused.
//!
//! # The hand-over
//!
//! The manager passes the sockets as descriptors 3, 4, ... and says so in three variables of the
//! environment: `LISTEN_PID` (the process they are for), `LISTEN_FDS` (how many) and
//! `LISTEN_FDNAMES` (their `FileDescriptorName=`s, separated by `:`). The process reads no other
//! environment variable; these three are the one exception to "configuration is flags", because
//! the platform sets them, not an operator, and what they say can only be read there. The flag is
//! what makes the process read them: without it they are ignored (a process started by hand from
//! a shell that has them left over binds its own addresses).
//!
//! The sockets are taken by name: [`S3`] for the S3 API (`--listen`), and [`OCI`] for the OCI
//! front (`--oci-listen`) when there is one. A start is refused (exit status 2, as a restart
//! cannot mend it) if
//!
//! - `LISTEN_PID`, `LISTEN_FDS` or `LISTEN_FDNAMES` is not set, or `LISTEN_PID` is not this
//!   process (the environment is somebody else's);
//! - the manager passed other sockets than the start needs, or a name more than once;
//! - a descriptor is not a listening TCP stream socket; or
//! - the socket is bound to another address than the flag says (`--listen` and `ListenStream=`
//!   are two places for one address, and they have to agree).
//!
//! The descriptors are made close-on-exec and non-blocking. The three variables stay in the
//! environment: the process starts no child process (a test pins that, so that a child added
//! later is noticed), and so nothing can take them for its own.
//!
//! # The drain
//!
//! The process stops serving by dropping its listener, which closes its descriptor and nothing
//! else: the socket stays open in the manager, listening, with its queue. The process never
//! calls `shutdown` on it, which would be the manager's too.

use std::net::{SocketAddr, TcpListener};
use std::os::fd::{BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};

use rustix::fs::{FileType, fstat};
use rustix::io::{FdFlags, fcntl_setfd};
use rustix::net::sockopt::{socket_acceptconn, socket_type};
use rustix::net::{SocketType, getsockname};

use super::cli::Error;
use super::front::InvalidFlags;

/// Whether this process has taken the hand-over (see [`Sockets::from_process`]).
static TAKEN: AtomicBool = AtomicBool::new(false);

/// The name of the socket of the S3 API.
pub(super) const S3: &str = "s3";

/// The name of the socket of the OCI front.
pub(super) const OCI: &str = "oci";

/// The first descriptor the manager passes (`SD_LISTEN_FDS_START`).
const FIRST_FD: RawFd = 3;

/// What the manager said about the sockets: the three variables, as text.
#[derive(Clone, Debug, Default)]
pub(super) struct Passed {
    pub pid: Option<String>,
    pub fds: Option<String>,
    pub names: Option<String>,
}

impl Passed {
    /// The three variables of the environment.
    pub(super) fn from_env() -> Self {
        Self {
            pid: std::env::var("LISTEN_PID").ok(),
            fds: std::env::var("LISTEN_FDS").ok(),
            names: std::env::var("LISTEN_FDNAMES").ok(),
        }
    }
}

/// The sockets a start took from the manager, by the name of what they serve. A start that took
/// none has none, and binds its addresses.
#[derive(Debug, Default)]
pub(super) struct Sockets {
    s3: Option<TcpListener>,
    oci: Option<TcpListener>,
}

impl Sockets {
    /// The sockets for a start that serves `s3` and, if given, `oci`, taken from the environment
    /// of this process (see the module docs for the rules).
    ///
    /// The hand-over is taken once per process: a second call is refused, whatever the first
    /// found, since the descriptors are owned by whoever took them first.
    ///
    /// # Errors
    /// An [`InvalidFlags`] that says what the manager passed wrongly, or that they were taken.
    pub(super) fn from_process(s3: SocketAddr, oci: Option<SocketAddr>) -> Result<Self, Error> {
        if TAKEN.swap(true, Ordering::SeqCst) {
            return Err(refuse("the sockets were already taken by this process".to_owned()).into());
        }
        let passed = Passed::from_env();
        let me = std::process::id();
        Ok(Self::from_passed(&passed, me, FIRST_FD, s3, oci)?)
    }

    /// [`Sockets::from_process`] for what the manager said, the id of the process it said it to,
    /// and the number of the first descriptor it passed (3, for a manager).
    pub(super) fn from_passed(
        passed: &Passed,
        me: u32,
        first: RawFd,
        s3: SocketAddr,
        oci: Option<SocketAddr>,
    ) -> Result<Self, InvalidFlags> {
        let wanted: Vec<(&str, SocketAddr)> = std::iter::once((S3, s3))
            .chain(oci.map(|addr| (OCI, addr)))
            .collect();
        let names: Vec<&str> = wanted.iter().map(|(name, _)| *name).collect();
        let descriptors = plan(passed, me, first, &names).map_err(refuse)?;
        let mut taken = Vec::new();
        for (fd, (name, expected)) in descriptors.into_iter().zip(&wanted) {
            taken.push(take(fd, name, *expected).map_err(refuse)?);
        }
        let mut taken = taken.into_iter();
        Ok(Self {
            s3: taken.next(),
            oci: taken.next(),
        })
    }

    /// The listener of the S3 API: the socket the manager passed, or `address` bound now.
    ///
    /// # Errors
    /// The socket cannot be registered with the runtime, or `address` cannot be bound.
    pub(super) async fn s3_or_bind(
        &mut self,
        address: SocketAddr,
    ) -> Result<tokio::net::TcpListener, Error> {
        match self.s3.take() {
            Some(socket) => tokio::net::TcpListener::from_std(socket).map_err(|why| {
                format!("taking the socket {S3:?} from the service manager: {why}").into()
            }),
            None => tokio::net::TcpListener::bind(address)
                .await
                .map_err(|why| format!("binding {address}: {why}").into()),
        }
    }

    /// The socket of the OCI front, if the manager passed one.
    pub(super) fn oci(&mut self) -> Option<TcpListener> {
        self.oci.take()
    }
}

/// The refusal of a hand-over, for `--listen-from-systemd`.
fn refuse(why: String) -> InvalidFlags {
    InvalidFlags(format!("--listen-from-systemd: {why}"))
}

/// The descriptors of the sockets named `wanted`, in that order, out of what the manager said it
/// passed from the descriptor `first` up.
///
/// # Errors
/// Why what the manager said is not a hand-over of exactly these sockets to the process `me`.
pub(super) fn plan(
    passed: &Passed,
    me: u32,
    first: RawFd,
    wanted: &[&str],
) -> Result<Vec<RawFd>, String> {
    let pid = passed
        .pid
        .as_deref()
        .ok_or("LISTEN_PID is not set: the process was not started by a socket unit")?;
    if pid.parse::<u32>() != Ok(me) {
        return Err(format!(
            "LISTEN_PID is {pid:?} but this process is {me}: the sockets were passed to another process"
        ));
    }
    let fds = passed.fds.as_deref().ok_or("LISTEN_FDS is not set")?;
    let names = passed
        .names
        .as_deref()
        .ok_or("LISTEN_FDNAMES is not set: name each socket with FileDescriptorName=")?;
    let given: Vec<&str> = names.split(':').collect();
    let count = fds
        .parse::<usize>()
        .map_err(|_| format!("LISTEN_FDS is {fds:?}, not a number of sockets"))?;
    if count != given.len() {
        return Err(format!(
            "LISTEN_FDS is {count} but LISTEN_FDNAMES is {names:?}, {} names",
            given.len()
        ));
    }
    if given.len() != wanted.len() {
        return Err(format!(
            "the service manager passed {} sockets ({names}) but this start needs {} ({})",
            given.len(),
            wanted.len(),
            wanted.join(", ")
        ));
    }
    wanted
        .iter()
        .map(|name| {
            let mut found = (first..).zip(&given).filter(|(_, given)| *given == name);
            match (found.next(), found.next()) {
                (Some((fd, _)), None) => Ok(fd),
                _ => Err(format!(
                    "LISTEN_FDNAMES is {names:?}: it must name one socket {name:?} \
                     (FileDescriptorName={name})"
                )),
            }
        })
        .collect()
}

/// Takes the descriptor `fd` as the listener named `name`, which has to serve `expected`.
///
/// A descriptor that is refused is left as it is: it is not closed.
///
/// This is the one place in the crate that is allowed `unsafe` (the crate denies it, and a test
/// pins that this is the only allowance): a number the manager passed becomes an owner here.
///
/// # Errors
/// Why the descriptor is not a listening TCP stream socket bound to `expected`.
#[allow(unsafe_code)]
pub(super) fn take(fd: RawFd, name: &str, expected: SocketAddr) -> Result<TcpListener, String> {
    // SAFETY: the borrow only asks the system what `fd` is (`fstat`, `getsockopt`, `getsockname`),
    // all of which answer `EBADF` for a number that is not open, which is how a closed descriptor
    // is refused; it is not kept, and nothing is closed through it.
    let bound = check(unsafe { BorrowedFd::borrow_raw(fd) }, fd, name)?;
    if bound != expected {
        return Err(format!(
            "the socket {name:?} (descriptor {fd}) is bound to {bound} but this start serves \
             {expected}: ListenStream= and the address of the flag must be one address"
        ));
    }
    // SAFETY: `fd` is open and is a listening TCP socket (`check`). It comes from the service
    // manager through LISTEN_FDS and LISTEN_FDNAMES, and LISTEN_PID named this process (`plan`),
    // so the manager passed it to this process alone; `Sockets::from_process` takes the hand-over
    // once per process, so no other owner is made of it, and the owner made here closes it once.
    let listener = TcpListener::from(unsafe { OwnedFd::from_raw_fd(fd) });
    // The socket of a manager is not close-on-exec, and is blocking.
    fcntl_setfd(&listener, FdFlags::CLOEXEC)
        .map_err(std::io::Error::from)
        .and_then(|()| listener.set_nonblocking(true))
        .map_err(|why| format!("the socket {name:?} (descriptor {fd}): {why}"))?;
    Ok(listener)
}

/// The address `fd` is bound to, if it is a listening TCP stream socket.
fn check(fd: BorrowedFd<'_>, raw: RawFd, name: &str) -> Result<SocketAddr, String> {
    let not = |what: &str| format!("the socket {name:?} (descriptor {raw}) is {what}");
    let status = fstat(fd).map_err(|why| not(&format!("not an open descriptor: {why}")))?;
    if FileType::from_raw_mode(status.st_mode) != FileType::Socket {
        return Err(not("not a socket"));
    }
    if socket_type(fd).ok() != Some(SocketType::STREAM) {
        return Err(not("not a stream socket"));
    }
    if !socket_acceptconn(fd).unwrap_or(false) {
        return Err(not("not a listening socket"));
    }
    // A socket that has a name (it was asked about above) but not an IP address: a unix socket.
    let bound = getsockname(fd)
        .ok()
        .and_then(|any| SocketAddr::try_from(any).ok());
    bound.ok_or_else(|| not("not a TCP socket (it is not bound to an IP address)"))
}

#[cfg(test)]
#[path = "activation_tests.rs"]
mod tests;
