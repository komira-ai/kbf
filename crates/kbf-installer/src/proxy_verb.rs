//! `kbf-installer proxy [--config PATH] [--check]`: hosts the `kbf-proxy` runtime
//! (docs/design/kbf-proxy-api.md section 11).
//!
//! The proxy is the one public port of a frontend host. This verb loads the `[proxy]` table of
//! `installer.toml`, takes the one listening socket a service manager activated for it,
//! binds the runtime to it and serves until SIGTERM or SIGINT. It runs as the unprivileged
//! service user and needs no root. On Linux it also starts the supervisor's first duty
//! ([`crate::proxy::supervisor`]): it adopts the frontend children a previous proxy left
//! running, by pid, start time and executable, and ends the startup hold into the one that
//! serves. The adoption itself never calls a child's admin endpoint: choosing between two
//! live children and starting a first child are not built; in those cases, and when an update
//! was in flight (the handover driver below resumes it, and the hold stays until it decides),
//! the startup hold stays in force, the proxy answers `UNAVAILABLE` once its bound is over and
//! `/readyz` says 503, and the reason is on the standard error. A child outlives the proxy: a
//! stop of the proxy signals the proxy alone (`KillMode=process` in `kbf-proxy.service`).
//!
//! **The update API.** `/v1/update/frontend` is the update API of [`crate::proxy::api`], run
//! by an actor on a thread of its own and called through `UpdateBridge`. The proxy
//! authenticates that route (the operator token) before it calls, bounds the body and the
//! time, and the API behind it does no check of its own: this verb is the only place the two
//! meet. Its seams are bound by [`crate::proxy::handover`] (`--update-dir` names the root
//! half's `status.json`, `--orders-dir` the order spool, the proxy's `state_dir` the request
//! map and the spool's counter, `confirm_window_s` the window its orders ask for). Behind it is
//! the handover driver ([`crate::proxy::handover_swap`]): once the proxy is bound and its
//! children adopted, `supervise` fills the driver's slot with the proxy's gate, the children's
//! process control and the proxy's secret, and the driver runs the swap on a thread of its own.
//! A `POST` it cannot take (an update open, no idle slot, no `releases/` directory) is refused
//! with 503 and nothing is written. If the actor or the swap thread dies the proxy is stopped
//! as a stop signal stops it and the verb exits 1, so the service manager starts it again.
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
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::os::fd::{AsFd as _, BorrowedFd, FromRawFd as _, OwnedFd, RawFd};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use kbf_proxy::Proxy;
use kbf_proxy::config::ProxyConfig;
use kbf_proxy::control::{UpdateHandler, UpdateReply, UpdateRequest};
use kbf_proxy::route::Target;
use kbf_proxy::slot::Slot;
use rustix::io::{Errno, FdFlags, fcntl_getfd, fcntl_setfd};
#[cfg(target_os = "linux")]
use rustix::net::sockopt::socket_acceptconn;
use tokio::signal::unix::{SignalKind, signal};

use crate::cli::ProxyArgs;
use crate::exit::Exit;
use crate::installer_toml::{InstallerToml, MAX_FILE_BYTES};
use crate::proxy::api::actor::ActorTiming;
use crate::proxy::api::core::{Core, Parts};
use crate::proxy::api::handler::{Handler, HandlerRequest};
use crate::proxy::handover::{self, HandoverConfig, HandoverError};
use crate::proxy::handover_swap::{GateSlot, SlotAddrs};
#[cfg(target_os = "linux")]
use crate::proxy::handover_swap::{Host, admin_secret};

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

impl From<HandoverError> for VerbError {
    /// An order spool or state directory the service user cannot use is the operator's to fix:
    /// exit 2, which `kbf-proxy.service` does not restart. A program that cannot be identified
    /// is not.
    fn from(e: HandoverError) -> Self {
        let text = format!("update API: {e}");
        match e {
            HandoverError::Orders { .. } => VerbError::Usage(text),
            HandoverError::Binary(_) => VerbError::Failed(text),
        }
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

/// What the verb takes from `installer.toml`.
#[derive(Debug)]
pub struct Loaded {
    /// The `[proxy]` table.
    pub proxy: ProxyConfig,
    /// `confirm_window_s`: the confirm window the update API's orders ask for.
    pub confirm_window_s: u32,
    /// `fetch_timeout_s`: how long the root half's fetch may take, which sizes the update
    /// API's staging deadline.
    pub fetch_timeout_s: u32,
}

/// Reads `path` and returns what the verb takes from it, checked: every refusal of
/// `installer.toml` and of the `[proxy]` table itself, and a file with no `[proxy]` table at
/// all.
pub fn load(path: &Path) -> Result<Loaded, VerbError> {
    let usage = |why: String| VerbError::Usage(format!("{}: {why}", path.display()));
    let mut text = String::new();
    std::fs::File::open(path)
        .and_then(|file| {
            file.take(MAX_FILE_BYTES as u64 + 1)
                .read_to_string(&mut text)
        })
        .map_err(|e| usage(e.to_string()))?;
    let installer = InstallerToml::parse(&text).map_err(|e| usage(e.to_string()))?;
    let proxy = installer
        .proxy
        .ok_or_else(|| usage("has no [proxy] table".to_owned()))?;
    Ok(Loaded {
        proxy,
        confirm_window_s: installer.confirm_window_s,
        fetch_timeout_s: installer.fetch_timeout_s,
    })
}

/// Where the update API reads and writes: the flags and the file.
fn handover_config(args: &ProxyArgs, loaded: &Loaded) -> HandoverConfig {
    let table = loaded.proxy.upstream;
    let at = |port: u16| SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let admin = |slot| at(table.admin_port(slot));
    let operator = |slot| at(table.port(slot, Target::Operator));
    HandoverConfig {
        slots: SlotAddrs {
            admin: [admin(Slot::A), admin(Slot::B)],
            operator: [operator(Slot::A), operator(Slot::B)],
        },
        operator_token_file: loaded.proxy.operator_token_file.clone(),
        gate: GateSlot::new(),
        state_dir: loaded.proxy.state_dir.clone(),
        orders_dir: args.orders_dir.clone(),
        status_file: args.update_dir.join("status.json"),
        confirm_window_s: loaded.confirm_window_s,
        fetch_timeout_s: loaded.fetch_timeout_s,
    }
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
        self.begin(Box::new(drain))
    }

    /// The verb has to stop and no signal asked it to (the update API's actor died): the same
    /// drain and the same bound as for a first signal. When a stop is already under way its
    /// bound is running and this does nothing, because it is not a second signal and must not
    /// end the drain at once. The thread that enforces the bound is returned for a test to join.
    ///
    /// `drain` is boxed so that this is one function: a copy for each closure type would
    /// have each of the two paths covered by a different copy.
    fn failed(self: &Arc<Self>, drain: Box<dyn FnOnce() + '_>) -> Option<JoinHandle<()>> {
        if self.seen.swap(true, Ordering::SeqCst) {
            return None;
        }
        Some(self.begin(drain))
    }

    fn begin(&self, drain: Box<dyn FnOnce() + '_>) -> JoinHandle<()> {
        drain();
        let (bound, exit) = (self.bound, self.exit);
        std::thread::spawn(move || {
            std::thread::sleep(bound);
            exit(1);
        })
    }
}

/// The update API's actor died. Remember it, and stop the proxy as a first stop signal does
/// (see [`Stop::failed`]): `outcome` then turns the clean return of the runtime into a failure.
fn actor_died(stop: &Arc<Stop>, died: &AtomicBool, drain: impl FnOnce()) {
    died.store(true, Ordering::SeqCst);
    drop(stop.failed(Box::new(drain)));
}

/// What the verb returns once the runtime has returned. A stop the actor's death asked for
/// ends the runtime cleanly (`run` is `Ok`), and that would be exit 0, which
/// `Restart=on-failure` does not restart: a proxy without its update API would stay down.
fn outcome(ran: std::io::Result<()>, died: &AtomicBool) -> Result<(), VerbError> {
    ran?;
    if died.load(Ordering::SeqCst) {
        return Err(VerbError::Failed(
            "the update API stopped: the proxy was stopped to be started again".to_owned(),
        ));
    }
    Ok(())
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

/// The update API behind the proxy's update route: the [`Handler`] of the API, called with the
/// proxy's request and answering with the proxy's reply (the fields are the same, and so are
/// the header types). It checks nothing: the proxy authenticated the caller before it called
/// (see the module doc).
struct UpdateBridge(Handler);

impl UpdateHandler for UpdateBridge {
    async fn handle(&self, request: UpdateRequest) -> UpdateReply {
        let reply = self
            .0
            .handle(HandlerRequest {
                method: request.method,
                path: request.path,
                body: request.body,
            })
            .await;
        UpdateReply {
            status: reply.status,
            headers: reply.headers,
            body: reply.body,
        }
    }
}

/// Starts the update API's actor over `parts`. If the actor dies, `drain` stops the proxy
/// ([`actor_died`]).
fn start_updates(
    parts: Parts,
    stop: &Arc<Stop>,
    died: &Arc<AtomicBool>,
    drain: &Arc<dyn Fn() + Send + Sync>,
) -> std::io::Result<UpdateBridge> {
    let (stop, died, drain) = (Arc::clone(stop), Arc::clone(died), Arc::clone(drain));
    let on_death = move || actor_died(&stop, &died, &*drain);
    Handler::start(Core::new(parts), ActorTiming::default(), on_death).map(UpdateBridge)
}

/// What the verb says on the standard error about the update API once it is built.
fn update_line(parts: &Parts, handover: &HandoverConfig) -> String {
    let driver = parts
        .supervisor
        .start_blocked()
        .map_or("handover ready".to_owned(), |why| {
            format!("a POST is refused: {why}")
        });
    format!(
        "update API on (status {}, orders {}, staging deadline {} s); {driver}",
        handover.status_file.display(),
        handover.orders_dir.display(),
        parts.config.staging_deadline.secs(),
    )
}

/// Serves on `listener` until a stop signal. The signal handlers are registered before the
/// proxy serves, so none is lost to the default action. The update API's seams are built
/// first: an order spool or a state directory the service user cannot use is a usage error
/// before anything is bound.
fn serve(
    config: ProxyConfig,
    handover: &HandoverConfig,
    listener: OwnedFd,
) -> Result<(), VerbError> {
    #[cfg(target_os = "linux")]
    let (state_dir, proxy_auth_file, upstream) = (
        config.state_dir.clone(),
        config.proxy_auth_file.clone(),
        config.upstream,
    );
    let parts = handover::parts(handover)?;
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
    let died = Arc::new(AtomicBool::new(false));
    // What stops the proxy, for a signal and for the death of the update API alike.
    let drain: Arc<dyn Fn() + Send + Sync> = {
        let handle = handle.clone();
        Arc::new(move || handle.shutdown())
    };
    for mut signals in [term?, int?] {
        let (drain, stop) = (Arc::clone(&drain), Arc::clone(&stop));
        runtime.spawn(async move {
            while signals.recv().await.is_some() {
                drop(stop.signalled(&*drain));
            }
        });
    }
    // A closed standard error (a journal that went away) must not stop the proxy from serving.
    #[cfg(target_os = "linux")]
    let _supervised = {
        let on_death: Arc<dyn Fn(&str) + Send + Sync> = {
            let (stop, died, drain) = (Arc::clone(&stop), Arc::clone(&died), Arc::clone(&drain));
            Arc::new(move |why| handover_died(why, &stop, &died, &*drain))
        };
        supervise(
            &handle,
            &state_dir,
            &proxy_auth_file,
            upstream,
            &handover.gate,
            on_death,
        )
    };
    let note = update_line(&parts, handover);
    let updates = start_updates(parts, &stop, &died, &drain)?;
    let _ = writeln!(std::io::stderr(), "kbf-installer: proxy: {note}");
    let _ = writeln!(std::io::stderr(), "kbf-installer: proxy serving");
    outcome(runner.run(updates), &died)
}

/// Adopts the children a previous proxy left and reports what it did on the standard error,
/// then tells the handover driver what it needs of the bound proxy (`gate`, filled once).
/// The gate it returns must live as long as the proxy: it keeps the hold guard of an adoption
/// that could not finish.
#[cfg(target_os = "linux")]
fn supervise(
    handle: &kbf_proxy::ProxyHandle,
    state_dir: &Path,
    proxy_auth_file: &Path,
    upstream: kbf_proxy::config::UpstreamTable,
    slot: &GateSlot,
    on_death: Arc<dyn Fn(&str) + Send + Sync>,
) -> Arc<crate::proxy::gate_real::ProxyGate> {
    use crate::layout::Layout;
    use crate::proxy::child_real::ProcChild;
    use crate::proxy::supervisor::{Startup, adopt_children, children_dir, frontend_argv};

    let gate = Arc::new(crate::proxy::gate_real::ProxyGate::new(handle.clone()));
    let layout = Layout::linux(kbf_host::Root::host());
    let releases_dir = layout.join(&layout.releases());
    let child = Arc::new(ProcChild::new(
        layout,
        children_dir(state_dir),
        frontend_argv(proxy_auth_file, upstream),
    ));
    let (line, active) = match adopt_children(&gate, &child, state_dir) {
        Startup::Adopted { active } => (
            format!("adopted the frontend child in slot {active:?}"),
            active,
        ),
        Startup::Held(why) => (
            format!("no frontend child adopted, new streams stay held: {why}"),
            crate::proxy::model::Slot::A,
        ),
    };
    let _ = writeln!(std::io::stderr(), "kbf-installer: proxy: {line}");
    let recorder = Arc::clone(&child);
    slot.fill(Host {
        gate: gate.clone(),
        child,
        recorded: Arc::new(move |slot| recorder.recorded(slot).map(|r| r.ident)),
        active,
        releases_dir,
        secret: admin_secret(&handle.admin_headers()),
        on_death,
    });
    gate
}

/// The handover driver's swap thread died: say why, then stop the proxy as the death of the
/// update API's actor does (the verb exits 1 and the unit restarts it, and the restart
/// adopts what the swap left).
fn handover_died(why: &str, stop: &Arc<Stop>, died: &AtomicBool, drain: impl FnOnce()) {
    let _ = writeln!(
        std::io::stderr(),
        "kbf-installer: proxy: the handover driver died: {why}"
    );
    actor_died(stop, died, drain);
}

/// The verb. With `--check`, the table is loaded and nothing is bound.
pub fn run(args: &ProxyArgs) -> Result<(), VerbError> {
    let loaded = load(&args.config)?;
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
    let handover = handover_config(args, &loaded);
    serve(loaded.proxy, &handover, listener)
}

#[cfg(test)]
#[path = "proxy_verb_tests.rs"]
mod tests;
