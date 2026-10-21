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
//! The start migrates before it runs anything: the installed release is brought to the
//! `state_schema` it declares on the database `installer.toml` names (`database`, a URL
//! without a password) before its child starts ([`crate::proxy::supervisor::host_migrator`]).
//! A frontend host runs on that database, so a file with no usable `database` is refused
//! (exit 2, `--check` included) rather than started without a migration; the URL is never
//! quoted, logged or passed in an argument vector.
//!
//! This module and the root helper (`helper/`, `run.rs`) share a binary and nothing else:
//! `kbf-integration-test`'s layering test fails if the root half ever names `kbf_proxy` or this module.
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
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::os::fd::{AsFd as _, BorrowedFd, FromRawFd as _, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
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
use crate::database::{Database, DatabaseError};
use crate::exit::Exit;
use crate::installer_toml::{InstallerToml, MAX_FILE_BYTES};
use crate::layout::{Layout, Platform};
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
    loaded_from(path, &read_config(path)?)
}

/// The text of the configuration at `path`, at most `MAX_FILE_BYTES`. The verb reads it once:
/// `--config` may be a pipe, which cannot be read twice.
fn read_config(path: &Path) -> Result<String, VerbError> {
    let mut text = String::new();
    std::fs::File::open(path)
        .and_then(|file| {
            file.take(MAX_FILE_BYTES as u64 + 1)
                .read_to_string(&mut text)
        })
        .map_err(|e| VerbError::Usage(format!("{}: {e}", path.display())))?;
    Ok(text)
}

/// What the verb takes from `text`, the configuration read from `path`.
fn loaded_from(path: &Path, text: &str) -> Result<Loaded, VerbError> {
    let usage = |why: String| VerbError::Usage(format!("{}: {why}", path.display()));
    let installer = InstallerToml::parse(text).map_err(|e| usage(e.to_string()))?;
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
fn handover_config(args: &ProxyArgs, loaded: &Loaded, layout: &Layout) -> HandoverConfig {
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
        installer_toml: config_path(args, Platform::current()),
        gate: GateSlot::new(),
        state_dir: loaded.proxy.state_dir.clone(),
        orders_dir: args.orders_dir_in(layout),
        status_file: args.update_dir_in(layout).join("status.json"),
        confirm_window_s: loaded.confirm_window_s,
        fetch_timeout_s: loaded.fetch_timeout_s,
    }
}

/// The state database `installer.toml` at `path` names: every host that runs the proxy has
/// one, so a file without a usable `database` is a usage error, in words that never quote the
/// URL (the reader's own: a password in it is refused too).
pub fn state_database(path: &Path) -> Result<Database, VerbError> {
    database_of(Database::from_installer_toml(path))
}

fn database_of(found: Result<Database, DatabaseError>) -> Result<Database, VerbError> {
    found.map_err(|e| {
        VerbError::Usage(format!(
            "the proxy needs the state database to migrate before it starts a release: {e}"
        ))
    })
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
    database: Database,
    handover: &HandoverConfig,
    listener: OwnedFd,
    root: &kbf_host::Root,
) -> Result<(), VerbError> {
    #[cfg(target_os = "linux")]
    let supervise_with = Supervise {
        database,
        state_dir: config.state_dir.clone(),
        proxy_auth_file: config.proxy_auth_file.clone(),
        upstream: config.upstream,
        operator_token_file: config.operator_token_file.clone(),
        status: handover.status_file.clone(),
    };
    // Off Linux nothing supervises the proxy, so neither the database nor the host root is used.
    #[cfg(not(target_os = "linux"))]
    let _ = (database, root);
    let parts = handover::parts(handover)?;
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
    let _supervised = supervise(
        &handle,
        supervise_with,
        root,
        &handover.gate,
        death_hook(&stop, &died, &drain),
    );
    let note = update_line(&parts, handover);
    let updates = start_updates(parts, &stop, &died, &drain)?;
    let _ = writeln!(std::io::stderr(), "kbf-installer: proxy: {note}");
    let _ = writeln!(std::io::stderr(), "kbf-installer: proxy serving");
    outcome(runner.run(updates), &died)
}

/// Where the supervisor finds what it reads.
#[cfg(target_os = "linux")]
struct Supervise {
    /// The state database a release is migrated on before its child starts.
    database: Database,
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
    thread: Option<JoinHandle<Arc<crate::proxy::gate_real::ProxyGate>>>,
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
/// what it did on the standard error, then tells the handover driver what it needs of the bound
/// proxy (`slot`, filled once, after the start has ended: until then the driver says the proxy
/// is not bound). `root` is the host root the layout is over.
#[cfg(target_os = "linux")]
fn supervise(
    handle: &kbf_proxy::ProxyHandle,
    with: Supervise,
    root: &kbf_host::Root,
    slot: &GateSlot,
    on_death: Arc<dyn Fn(&str) + Send + Sync>,
) -> Supervised {
    use crate::layout::Layout;
    use crate::proxy::boot::Retry;
    use crate::proxy::child_real::ProcChild;
    use crate::proxy::runtime::SystemClock;
    use crate::proxy::supervisor::{Startup, children_dir, frontend_argv};

    let clock = Arc::new(SystemClock::new());
    let (handle, wait) = (handle.clone(), Arc::clone(&clock));
    let (root, slot) = (root.clone(), slot.clone());
    let thread = std::thread::Builder::new()
        .name("supervisor".into())
        .spawn(move || {
            let gate = Arc::new(crate::proxy::gate_real::ProxyGate::new(handle.clone()));
            let layout = Layout::linux(root);
            let releases_dir = layout.join(&layout.releases());
            let child = Arc::new(ProcChild::new(
                layout,
                children_dir(&with.state_dir),
                frontend_argv(&with.proxy_auth_file, with.upstream),
            ));
            let started = start_with(&handle, &gate, &*wait, &child, with, Retry::boot());
            let (active, held) = match &started {
                Ok(Startup::Adopted { active }) => (*active, None),
                Ok(Startup::Held(why)) | Err(why) => {
                    (crate::proxy::model::Slot::A, Some(why.clone()))
                }
            };
            let line = startup_line(started);
            let _ = writeln!(std::io::stderr(), "kbf-installer: proxy: {line}");
            let recorder = Arc::clone(&child);
            slot.fill(Host {
                gate: gate.clone(),
                child,
                recorded: recorded_by(&recorder),
                active,
                held,
                releases_dir,
                secret: admin_secret(&handle.admin_headers()),
                on_death,
            });
            gate
        })
        .ok();
    Supervised { clock, thread }
}

/// The start over `child`: the admin client, the migration hook ([`host_migrator`] on the
/// database of `with`) and the report. The handover driver is handed the same `child`.
#[cfg(target_os = "linux")]
fn start_with(
    handle: &kbf_proxy::ProxyHandle,
    gate: &crate::proxy::gate_real::ProxyGate,
    clock: &dyn crate::proxy::ports::Clock,
    child: &crate::proxy::child_real::ProcChild,
    with: Supervise,
    retry: crate::proxy::boot::Retry,
) -> Result<crate::proxy::supervisor::Startup, String> {
    use crate::proxy::boot::FileReport;
    use crate::proxy::model::Timing;
    use crate::proxy::root_orders::StatusFile;
    use crate::proxy::supervisor::{Boot, adopt_children, child_admin, host_migrator};

    child_admin(handle, with.upstream, with.operator_token_file).map(|admin| {
        let report = FileReport::new(&with.state_dir);
        let facts = StatusFile::new(&with.status);
        let migrator = host_migrator(with.database, &facts, child);
        let boot = Boot {
            admin: &admin,
            clock,
            migrator: &migrator,
            report: &report,
            status: &with.status,
            timing: Timing::frontend(),
            retry,
        };
        adopt_children(gate, child, &with.state_dir, &boot)
    })
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

/// What the handover driver runs when its swap thread dies: [`handover_died`] over this verb's
/// stop, death flag and drain.
#[cfg(target_os = "linux")]
fn death_hook(
    stop: &Arc<Stop>,
    died: &Arc<AtomicBool>,
    drain: &Arc<dyn Fn() + Send + Sync>,
) -> Arc<dyn Fn(&str) + Send + Sync> {
    let (stop, died, drain) = (Arc::clone(stop), Arc::clone(died), Arc::clone(drain));
    Arc::new(move |why| handover_died(why, &stop, &died, &*drain))
}

/// The process the children's records name for a slot, as the driver asks for it: what a
/// restarted proxy adopts.
#[cfg(target_os = "linux")]
fn recorded_by(
    child: &Arc<crate::proxy::child_real::ProcChild>,
) -> Arc<dyn Fn(crate::proxy::model::Slot) -> Option<crate::proxy::model::ProcIdent> + Send + Sync>
{
    let child = Arc::clone(child);
    Arc::new(move |slot| child.recorded(slot).map(|recorded| recorded.ident))
}

/// The handover driver's swap thread died: say why, then stop the proxy as the death of the
/// update API's actor does (the verb exits 1 and the unit restarts it, and the restart
/// adopts what the swap left).
#[cfg(target_os = "linux")]
fn handover_died(why: &str, stop: &Arc<Stop>, died: &AtomicBool, drain: impl FnOnce()) {
    let _ = writeln!(
        std::io::stderr(),
        "kbf-installer: proxy: the handover driver died: {why}"
    );
    actor_died(stop, died, drain);
}

/// The verb. With `--check`, the table is loaded and nothing is bound.
pub fn run(args: &ProxyArgs, root: &Path) -> Result<(), VerbError> {
    hold_reload_signal();
    let path = config_path(args, Platform::current());
    let text = read_config(&path)?;
    let loaded = loaded_from(&path, &text)?;
    let database = database_of(Database::from_installer_text(
        &path.display().to_string(),
        &text,
    ))?;
    if args.check {
        return Ok(());
    }
    let root = kbf_host::Root::new(root).map_err(|e| VerbError::Usage(format!("--root: {e}")))?;
    let listen_pid = std::env::var("LISTEN_PID").ok();
    let listen_fds = std::env::var("LISTEN_FDS").ok();
    let listener = activated(
        listen_pid.as_deref(),
        listen_fds.as_deref(),
        std::process::id(),
        FIRST_FD,
    )?;
    let handover = handover_config(
        args,
        &loaded,
        &Layout::new(root.clone(), Platform::current()),
    );
    serve(loaded.proxy, database, &handover, listener, &root)
}

#[cfg(test)]
#[path = "proxy_verb_tests.rs"]
mod tests;
