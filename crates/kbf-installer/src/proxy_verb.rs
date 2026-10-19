//! `kbf-installer proxy [--config PATH] [--check]`: hosts the `kbf-proxy` runtime
//! (docs/design/kbf-proxy-api.md section 11).
//!
//! The proxy is the one public port of a frontend host. This verb loads the `[proxy]` table of
//! `installer.toml`, takes the one listening socket a service manager activated for it,
//! binds the runtime to it and serves until SIGTERM or SIGINT. It runs as the unprivileged
//! service user: it needs no root, writes nothing on the host and never calls a child's admin
//! endpoint or acts on a child; it only polls the active child's `/readyz`. It does not
//! supervise either: the supervisor that adopts the children and ends the proxy's startup
//! hold is the update work that follows, and until it exists the proxy answers `UNAVAILABLE`
//! once the hold is over and `/readyz` says 503.
//!
//! This module and the root helper (`helper/`, `run.rs`) share a binary and nothing else:
//! `kbf-it`'s layering test fails if the root half ever names `kbf_proxy` or this module.
//!
//! The only environment variables read are the service manager's socket-activation markers,
//! `LISTEN_PID` and `LISTEN_FDS`; everything else is the `[proxy]` table.
//!
//! `--check` loads the whole file the way a run does, so a bad key outside `[proxy]` is
//! refused (exit 2) too. A stop signal drains for at most 5 s; the process ends within
//! [`STOP_BOUND`] of the first one is seen, whatever the drain is doing, and at once on a
//! second. SIGTERM and SIGINT are unblocked at start, so a launcher's inherited mask cannot
//! hide them.

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::os::fd::{AsFd as _, BorrowedFd, FromRawFd as _, OwnedFd, RawFd};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use kbf_proxy::Proxy;
use kbf_proxy::config::ProxyConfig;
use kbf_proxy::control::NoUpdates;
use rustix::io::{Errno, FdFlags, fcntl_getfd, fcntl_setfd};
#[cfg(target_os = "linux")]
use rustix::net::sockopt::socket_acceptconn;
use tokio::signal::unix::{SignalKind, signal};

use crate::cli::ProxyArgs;
use crate::exit::Exit;
use crate::installer_toml::{InstallerToml, MAX_FILE_BYTES};

/// The first descriptor a service manager passes (`SD_LISTEN_FDS_START`).
pub const FIRST_FD: RawFd = 3;

/// Why the verb stopped.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum VerbError {
    /// The file is unreadable or invalid, or the service was not started as the verb needs:
    /// exit 2.
    #[error("{0}")]
    Usage(String),
    /// The proxy could not start or stopped: exit 1.
    #[error("{0}")]
    Failed(String),
}

impl From<std::io::Error> for VerbError {
    fn from(e: std::io::Error) -> Self {
        VerbError::Failed(e.to_string())
    }
}

impl From<Errno> for VerbError {
    fn from(e: Errno) -> Self {
        VerbError::Failed(e.to_string())
    }
}

impl VerbError {
    /// The proxy could not be bound to its socket.
    fn starting(e: std::io::Error) -> Self {
        VerbError::Failed(format!("cannot start the proxy: {e}"))
    }

    /// The exit code the error maps to.
    pub fn exit(&self) -> Exit {
        match self {
            VerbError::Usage(_) => Exit::Usage,
            VerbError::Failed(_) => Exit::Failed,
        }
    }

    /// The `error` word of `--json`.
    pub fn code(&self) -> &'static str {
        match self {
            VerbError::Usage(_) => "usage",
            VerbError::Failed(_) => "failed",
        }
    }
}

/// Reads `path` and returns its `[proxy]` table, checked: every refusal of `installer.toml`
/// and of the table itself, and a file with no `[proxy]` table at all.
pub fn load(path: &Path) -> Result<ProxyConfig, VerbError> {
    let usage = |why: String| VerbError::Usage(format!("{}: {why}", path.display()));
    let mut text = String::new();
    std::fs::File::open(path)
        .and_then(|file| {
            file.take(MAX_FILE_BYTES as u64 + 1)
                .read_to_string(&mut text)
        })
        .map_err(|e| usage(e.to_string()))?;
    InstallerToml::parse(&text)
        .map_err(|e| usage(e.to_string()))?
        .proxy
        .ok_or_else(|| usage("has no [proxy] table".to_owned()))
}

/// The one activated socket, or why there is not exactly one. `listen_pid` and `listen_fds`
/// are the markers as the environment has them, `pid` is this process, and `first` the
/// descriptor numbering starts at. It must be a listening TCP socket; the verb takes it, makes
/// it close-on-exec (the children the supervisor starts must not hold the public port) and
/// returns it.
pub fn activated(
    listen_pid: Option<&str>,
    listen_fds: Option<&str>,
    pid: u32,
    first: RawFd,
) -> Result<OwnedFd, VerbError> {
    let usage = |why: &str| VerbError::Usage(format!("socket activation: {why}"));
    if listen_pid.and_then(|p| p.parse::<u32>().ok()) != Some(pid) {
        return Err(usage(
            "LISTEN_PID is not this process: the proxy takes its port from the service manager",
        ));
    }
    match listen_fds.and_then(|n| n.parse::<u32>().ok()) {
        Some(1) => {}
        _ => return Err(usage("LISTEN_FDS must be exactly 1: one public port")),
    }
    // SAFETY: the service manager passed descriptor `first` to this process (LISTEN_PID names
    // it) and nothing else here has opened it; it is only borrowed for the checks.
    let borrowed = unsafe { BorrowedFd::borrow_raw(first) };
    fcntl_getfd(borrowed).map_err(|_| usage("the descriptor is not open"))?;
    if !is_listening(borrowed) {
        return Err(usage("the descriptor is not a listening socket"));
    }
    // SAFETY: as above, and the checks show it is an open descriptor; this is its one owner.
    let owned = unsafe { OwnedFd::from_raw_fd(first) };
    let listener = TcpListener::from(owned);
    listener
        .local_addr()
        .map_err(|_| usage("the descriptor is not a TCP socket"))?;
    let owned = OwnedFd::from(listener);
    fcntl_setfd(owned.as_fd(), FdFlags::CLOEXEC)?;
    Ok(owned)
}

/// Whether `SO_ACCEPTCONN` says the socket is listening; a descriptor that is not a socket
/// answers no.
#[cfg(target_os = "linux")]
fn is_listening(fd: BorrowedFd<'_>) -> bool {
    socket_acceptconn(fd).unwrap_or(false)
}

/// The same question as on Linux, answered from the TCP state. rustix has `socket_acceptconn`
/// only on Linux-like targets, and macOS cannot answer it: XNU's `sogetoptlock` serves no
/// `SO_ACCEPTCONN` (the flag lives in `so_options` and `listen` sets it, but the read side has
/// no case for it, so `getsockopt` fails with `ENOPROTOOPT`). What XNU does serve is
/// `TCP_CONNECTION_INFO`, whose `tcpi_state` is the socket's `t_state`; `TCPS_LISTEN` is 1.
/// A descriptor that is not a TCP socket (not open, a Unix socket, UDP) makes the call fail and
/// answers no.
#[cfg(not(target_os = "linux"))]
fn is_listening(fd: BorrowedFd<'_>) -> bool {
    use std::os::fd::AsRawFd as _;
    /// `TCPS_LISTEN` in XNU's `netinet/tcp_fsm.h`; `libc` does not carry the state names.
    const TCPS_LISTEN: u8 = 1;
    // SAFETY: all-zero is a valid `tcp_connection_info` (integers only).
    let mut info: libc::tcp_connection_info = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::tcp_connection_info>() as libc::socklen_t;
    // SAFETY: `fd` is open for the call; `info` and `len` are live and `len` is the size of
    // `info`, which is all `getsockopt` writes to.
    let rc = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::IPPROTO_TCP,
            libc::TCP_CONNECTION_INFO,
            (&raw mut info).cast(),
            &raw mut len,
        )
    };
    rc == 0 && info.tcpi_state == TCPS_LISTEN
}

/// How long after the first stop signal the process may live. The proxy drains open streams
/// for 5 s (`kbf-proxy`'s `SHUTDOWN_DRAIN`) and then returns; this is that plus a margin for
/// the runtime to wind down, after which the process exits whatever is left.
const STOP_BOUND: Duration = Duration::from_secs(10);

/// What a stop signal does. The first asks the proxy to drain and starts a clock on a thread
/// of its own, so a drain that never finishes ends the process within [`STOP_BOUND`] of the
/// signal being seen. The signal itself is received by a task on the proxy's runtime: the
/// bound does not cover a runtime too stuck to run that task. Any later signal is an operator
/// who is done waiting and ends the process at once. Both exits are status 1: the stop was
/// not the clean drain.
struct Stop {
    seen: AtomicBool,
    bound: Duration,
    exit: fn(i32) -> !,
}

impl Stop {
    /// A stop signal arrived. `drain` asks the proxy to stop; it runs for the first signal
    /// only. The thread that enforces the bound is returned for a test to join.
    fn signalled(self: &Arc<Self>, drain: impl FnOnce()) -> JoinHandle<()> {
        if self.seen.swap(true, Ordering::SeqCst) {
            (self.exit)(1);
        }
        drain();
        let (bound, exit) = (self.bound, self.exit);
        std::thread::spawn(move || {
            std::thread::sleep(bound);
            exit(1);
        })
    }
}

/// Unblocks SIGTERM and SIGINT in this thread, and so in the runtime threads created after it:
/// the signal mask survives `exec`, and a launcher that blocked them would otherwise leave a
/// verb that no stop signal reaches.
fn unblock_stop_signals() {
    // SAFETY: `sigemptyset` and `sigaddset` fill a set owned here; `pthread_sigmask` reads it
    // and changes only this thread's mask.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut set);
        libc::sigaddset(&raw mut set, libc::SIGTERM);
        libc::sigaddset(&raw mut set, libc::SIGINT);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &raw const set, std::ptr::null_mut());
    }
}

/// Serves on `listener` until a stop signal. The signal handlers are registered before the
/// proxy serves, so none is lost to the default action.
fn serve(config: ProxyConfig, listener: OwnedFd) -> Result<(), VerbError> {
    unblock_stop_signals();
    let (handle, runner) = Proxy::bind(config, listener).map_err(VerbError::starting)?;
    let runtime = handle.runtime();
    let (term, int) = runtime.block_on(async {
        (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        )
    });
    let stop = Arc::new(Stop {
        seen: AtomicBool::new(false),
        bound: STOP_BOUND,
        exit: std::process::exit,
    });
    for mut signals in [term?, int?] {
        let (handle, stop) = (handle.clone(), Arc::clone(&stop));
        runtime.spawn(async move {
            while signals.recv().await.is_some() {
                drop(stop.signalled(|| handle.shutdown()));
            }
        });
    }
    // A closed standard error (a journal that went away) must not stop the proxy from serving.
    let _ = writeln!(std::io::stderr(), "kbf-installer: proxy serving");
    Ok(runner.run(NoUpdates)?)
}

/// The verb. With `--check`, the table is loaded and nothing is bound.
pub fn run(args: &ProxyArgs) -> Result<(), VerbError> {
    let config = load(&args.config)?;
    if args.check {
        return Ok(());
    }
    let listen_pid = std::env::var("LISTEN_PID").ok();
    let listen_fds = std::env::var("LISTEN_FDS").ok();
    let listener = activated(
        listen_pid.as_deref(),
        listen_fds.as_deref(),
        std::process::id(),
        FIRST_FD,
    )?;
    serve(config, listener)
}

#[cfg(test)]
#[path = "proxy_verb_tests.rs"]
mod tests;
