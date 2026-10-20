//! `kbf-installer proxy [--config PATH] [--check]`: hosts the `kbf-proxy` runtime
//! (docs/design/kbf-proxy-api.md section 11).
//!
//! The proxy is the one public port of a frontend host. This verb loads the `[proxy]` table of
//! `installer.toml`, takes the one listening socket a service manager activated for it,
//! binds the runtime to it and serves until SIGTERM or SIGINT. It runs as the unprivileged
//! service user and needs no root. On Linux it also starts the supervisor's first duty
//! ([`crate::proxy::supervisor`]): it adopts the frontend children a previous proxy left
//! running, by pid, start time and executable, and ends the startup hold into the one that
//! serves. It never calls a child's admin endpoint: resuming an update that was in flight,
//! choosing between two live children and starting a first child need the child's admin port
//! and the root half's orders, which are not built yet; in those cases the startup hold stays
//! in force, the proxy answers `UNAVAILABLE` once its bound is over and `/readyz` says 503,
//! and the reason is on the standard error. A child outlives the proxy: a stop of the proxy
//! signals the proxy alone (`KillMode=process` in `kbf-proxy.service`).
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
//! hide them. SIGHUP is held pending from the first line of the verb until its reload handler
//! is registered, so a reload asked for the moment the unit is active is delivered, not fatal.

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::os::fd::{AsFd as _, BorrowedFd, FromRawFd as _, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
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
use crate::layout::Platform;

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
    /// The stop the verb runs with: the bound is [`STOP_BOUND`] and the exit is the process's.
    fn production() -> Arc<Self> {
        Arc::new(Self {
            seen: AtomicBool::new(false),
            bound: STOP_BOUND,
            exit: std::process::exit,
        })
    }

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

/// Changes this thread's mask for `signals`: `how` is `SIG_BLOCK` or `SIG_UNBLOCK`.
fn change_mask(how: libc::c_int, signals: &[libc::c_int]) {
    // SAFETY: `sigemptyset` and `sigaddset` fill a set owned here; `pthread_sigmask` reads it
    // and changes only this thread's mask.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut set);
        for &signal in signals {
            libc::sigaddset(&raw mut set, signal);
        }
        libc::pthread_sigmask(how, &raw const set, std::ptr::null_mut());
    }
}

/// Unblocks SIGTERM and SIGINT in this thread, and so in the runtime threads created after it:
/// the signal mask survives `exec`, and a launcher that blocked them would otherwise leave a
/// verb that no stop signal reaches.
fn unblock_stop_signals() {
    change_mask(libc::SIG_UNBLOCK, &[libc::SIGTERM, libc::SIGINT]);
}

/// Holds SIGHUP pending from the start of the verb until its handler exists
/// ([`release_reload_signal`]). A reload can be asked for the moment the unit is active
/// (`ExecReload`), and until a handler is installed SIGHUP's default action ends the process.
/// A blocked signal is not lost: it is delivered, to the handler, when it is unblocked. The
/// runtime threads created in between inherit the block; the thread that unblocks is the one
/// that serves, and it stays alive for as long as the proxy does.
fn hold_reload_signal() {
    change_mask(libc::SIG_BLOCK, &[libc::SIGHUP]);
}

/// Unblocks SIGHUP: call it only once the handler is registered. It also undoes a block the
/// launcher left in the mask the verb inherited.
fn release_reload_signal() {
    change_mask(libc::SIG_UNBLOCK, &[libc::SIGHUP]);
}

/// Serves on `listener` until a stop signal. The signal handlers are registered before the
/// proxy serves, so none is lost to the default action.
fn serve(config: ProxyConfig, listener: OwnedFd, update_dir: &Path) -> Result<(), VerbError> {
    #[cfg(target_os = "linux")]
    let supervise_with = Supervise {
        state_dir: config.state_dir.clone(),
        proxy_auth_file: config.proxy_auth_file.clone(),
        upstream: config.upstream,
        operator_token_file: config.operator_token_file.clone(),
        status: update_dir.join("status.json"),
    };
    #[cfg(not(target_os = "linux"))]
    let _ = update_dir;
    unblock_stop_signals();
    let (handle, runner) = Proxy::bind(config, listener).map_err(VerbError::starting)?;
    let runtime = handle.runtime();
    let (term, int, hup) = runtime.block_on(async {
        (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
            signal(SignalKind::hangup()),
        )
    });
    let stop = Stop::production();
    for mut signals in [term?, int?] {
        let (handle, stop) = (handle.clone(), Arc::clone(&stop));
        runtime.spawn(async move {
            while signals.recv().await.is_some() {
                drop(stop.signalled(|| handle.shutdown()));
            }
        });
    }
    // SIGHUP re-reads the certificate, key and client CA (`systemctl reload`, which the renew
    // hook calls). It never stops the proxy: without this handler its default action would
    // end the process and cut every stream. A failed reload keeps the old files and counts
    // itself (`kbf_proxy_tls_reload_failed_total`); this verb installs no `tracing`
    // subscriber, so the one line that says it happened is written here, to the journal. The
    // reload reads files and parses X.509, so it runs on the blocking pool, not on a runtime
    // thread; signals that arrive while one runs coalesce, and the reloader runs one at a time.
    let mut hup = hup?;
    let reloading = handle.clone();
    runtime.spawn(async move {
        while hup.recv().await.is_some() {
            let handle = reloading.clone();
            drop(tokio::task::spawn_blocking(move || {
                let line = match handle.reload_tls() {
                    Ok(()) => "reloaded the TLS files".to_owned(),
                    Err(error) => {
                        format!("TLS files unusable, the previous ones are kept: {error}")
                    }
                };
                let _ = writeln!(std::io::stderr(), "kbf-installer: proxy: {line}");
            }));
        }
    });
    // The handler exists: a SIGHUP held since the start of the verb is delivered now.
    release_reload_signal();
    // A closed standard error (a journal that went away) must not stop the proxy from serving.
    #[cfg(target_os = "linux")]
    let _supervised = supervise(&handle, supervise_with);
    let _ = writeln!(std::io::stderr(), "kbf-installer: proxy serving");
    Ok(runner.run(NoUpdates)?)
}

/// Where the supervisor finds what it reads.
#[cfg(target_os = "linux")]
struct Supervise {
    state_dir: std::path::PathBuf,
    proxy_auth_file: std::path::PathBuf,
    upstream: kbf_proxy::config::UpstreamTable,
    operator_token_file: std::path::PathBuf,
    /// The root half's `status.json`.
    status: std::path::PathBuf,
}

/// The supervisor, on a thread of its own: starting a first child waits for it to be ready,
/// and the proxy must already be serving (its `/readyz`, its hold) while it does. Dropping
/// this ends the wait of a supervisor still running and joins it. The gate the thread returns
/// is dropped last: it keeps the hold guard of a start that could not finish.
#[cfg(target_os = "linux")]
struct Supervised {
    clock: Arc<crate::proxy::runtime::SystemClock>,
    thread: Option<JoinHandle<crate::proxy::gate_real::ProxyGate>>,
}

#[cfg(target_os = "linux")]
impl Drop for Supervised {
    fn drop(&mut self) {
        self.clock.stop();
        // `None` when the thread never started; the gate is dropped with the join's result.
        self.thread.take().into_iter().for_each(|thread| {
            drop(thread.join());
        });
    }
}

/// Adopts the children a previous proxy left, or starts the installed release, and reports
/// what it did on the standard error.
#[cfg(target_os = "linux")]
fn supervise(handle: &kbf_proxy::ProxyHandle, with: Supervise) -> Supervised {
    use crate::layout::Layout;
    use crate::proxy::boot::{FileReport, Retry};
    use crate::proxy::child_real::ProcChild;
    use crate::proxy::model::Timing;
    use crate::proxy::runtime::SystemClock;
    use crate::proxy::supervisor::{
        Boot, NoMigration, adopt_children, child_admin, children_dir, frontend_argv,
    };

    let clock = Arc::new(SystemClock::new());
    let (handle, wait) = (handle.clone(), Arc::clone(&clock));
    let thread = std::thread::Builder::new()
        .name("supervisor".into())
        .spawn(move || {
            let gate = crate::proxy::gate_real::ProxyGate::new(handle.clone());
            let layout = Layout::linux(kbf_host::Root::host());
            let child = ProcChild::new(
                layout,
                children_dir(&with.state_dir),
                frontend_argv(&with.proxy_auth_file, with.upstream),
            );
            let started =
                child_admin(&handle, with.upstream, with.operator_token_file).map(|admin| {
                    let report = FileReport::new(&with.state_dir);
                    let boot = Boot {
                        admin: &admin,
                        clock: &*wait,
                        migrator: &NoMigration,
                        report: &report,
                        status: &with.status,
                        timing: Timing::frontend(),
                        retry: Retry::boot(),
                    };
                    adopt_children(&gate, &child, &with.state_dir, &boot)
                });
            let line = startup_line(started);
            let _ = writeln!(std::io::stderr(), "kbf-installer: proxy: {line}");
            gate
        })
        .ok();
    Supervised { clock, thread }
}

/// What the supervisor tells the log about how the start went.
#[cfg(target_os = "linux")]
fn startup_line(started: Result<crate::proxy::supervisor::Startup, String>) -> String {
    use crate::proxy::supervisor::Startup;
    match started {
        Ok(Startup::Adopted { active }) => format!("the frontend child in slot {active:?} serves"),
        Ok(Startup::Held(why)) | Err(why) => {
            format!("no frontend child adopted, new streams stay held: {why}")
        }
    }
}

/// The table's file: `--config`, else the `installer.toml` `setup` writes on `platform`.
fn config_path(args: &ProxyArgs, platform: Platform) -> PathBuf {
    args.config
        .clone()
        .unwrap_or_else(|| PathBuf::from(platform.installer_toml()))
}

/// The verb. With `--check`, the table is loaded and nothing is bound.
pub fn run(args: &ProxyArgs) -> Result<(), VerbError> {
    hold_reload_signal();
    let config = load(&config_path(args, Platform::current()))?;
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
    serve(config, listener, &args.update_dir)
}

#[cfg(test)]
#[path = "proxy_verb_tests.rs"]
mod tests;
