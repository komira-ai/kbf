//! The daemon's main loop: one outbound mutual-TLS stream at a time to a server,
//! reconnecting when it ends, with the lease manager and the fence clock running
//! throughout, connected or not. Which server it tries next, and how long it waits
//! between rounds of attempts, is [`crate::connect`]'s: it never stops trying.
//!
//! A session sends Hello, which names this daemon process by an instance id drawn when
//! the daemon starts and the same on every stream (issue #140: the scheduler gives up at
//! once only the leases of the same process that a new stream leaves out), waits for
//! Welcome, then sends a Heartbeat at the interval Welcome names and handles what the
//! server sends: `HeartbeatAck` renews contact,
//! `LeaseOffer` is logged and runs nothing, `Start` runs a lease if it arrived within
//! the window it names (see `window`; a late one is dropped without a Result), on the
//! one runtime of the daemon's [`Runtimes`] that serves its kind (none: refused), and
//! `Cancel` kills a running lease. Results go out on the stream. Every Result is kept
//! until the server's `ResultAck` names its lease (issue #26): until then each
//! Heartbeat lists the lease in `running`, so the scheduler does not take it as lost,
//! and each new stream resends it right after Welcome, before its first Heartbeat. A
//! Result produced while disconnected is sent the same way. After those Results and
//! before the first Heartbeat, each stream sends the node's software status
//! (`NodeStatus`, see [`crate::status`]).
//!
//! **Drain.** [`Daemon::run_draining`] takes the stop signals as a channel. On the first
//! the daemon drains: its Hello says [`NodePhase::Draining`] (resent on the stream, and
//! first on every later one), so the server offers it no lease and keeps the ones it
//! holds while the heartbeats go on; a Start that still arrives is refused with a Result
//! the server runs again (`UNAVAILABLE`); the runs go on, report and are acknowledged as
//! always. It returns, so the process exits 0, once no lease runs and no Result is
//! unacknowledged, connected or not, however long that takes: a drain never ends
//! because time ran out. A deadline, if the operator sets one, only logs a warning with
//! the number of leases still running, once, and the wait goes on. A second signal is
//! the operator's hard stop: it returns at once, and the runs still going are abandoned
//! as [`Daemon::run`] abandons them.
//!
//! A runtime that switches a Mac's GUI session for a whole-machine lease hands the
//! daemon its phase ([`Daemon::with_phase`]; `docs/design/worker-protocol.md#node-phases`):
//! every Hello carries the newest, and a change resends the Hello on the stream. A
//! Welcome that names another lease epoch than the switching lease's term, or a
//! `Cancel` for a lease that is not running here, has every runtime kill that lease,
//! so a runtime that keeps a lease across its own restart (the whole-machine runtime
//! does) cleans up after one the server no longer holds.
//!
//! A driver whose report changes while the daemon runs (the native driver re-checks its
//! Xcodes, issue #164) hands the daemon a [`DriverReport`] channel
//! ([`Daemon::with_driver_report`]); a driver with more than one survey sends each
//! through its own part of one [`crate::DriverWatch`], which merges them, so no survey
//! overwrites another's. Its entries join the report the daemon was started
//! with, and its Xcodes go into `NodeStatus`. Each change is taken when it arrives: when
//! the report changed, the Hello is resent on the stream (the server then places by the
//! new report, issue #25), and `NodeStatus` is sent again either way. A change while no
//! stream is up is taken by the next stream's Hello.
//!
//! Each lease is remembered with the server's lease epoch at its Start and the action
//! the Start named (issue #137). Every Result echoes that action, so the server can
//! refuse one that answers another operation. A Welcome that names another epoch
//! comes from a server that never granted those leases and will never accept their
//! Results: before anything else on that stream, the daemon kills their runs and
//! forgets their Results, unsent. A lease id of the old epoch can then never be
//! mistaken for one the new server grants, as an unacknowledged Result or a run that
//! a new Start would otherwise be taken to resend.
//!
//! The fence and the Start window read a [`Clock`] that counts suspended time (issue
//! #78); tokio's timers do not, so the loop never sleeps longer than `recheck_every`
//! (1 s) while a deadline is pending. Whatever wakes the daemon (a message, a heartbeat
//! tick, a finished run, that recheck, or, between streams, a connection, a stream or
//! a Welcome arriving), the fence is checked first: after a resume past T, no running
//! lease survives to have its Result sent, and neither a Welcome for a Hello sent after
//! the resume nor an acknowledgement of a heartbeat sent after it renews the contact
//! first.
//!
//! A fence kills without waiting (see [`crate::lease`]): the loop goes on while a run
//! is slow to stop. Once contact is lost and the stream has acknowledged nothing for
//! 1.5 T of time awake, the stream is ended and the daemon connects again and says
//! Hello; the fenced leases are listed on the new stream until their runs stop and
//! report.
//!
//! A v1 assumption: the server acknowledges every Result. ResultAck is an addition
//! within protocol version 1 (worker.proto), so Welcome's version check does not rule
//! out a server built before it; such a server would leave every Result kept, resent
//! on every stream and listed in every Heartbeat for the daemon's lifetime. No server
//! without ResultAck has been released, which is why the version was not raised for it.

use std::future::Future;
use std::hash::{BuildHasher, RandomState};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::channel::mpsc::{UnboundedSender, unbounded};
use kbf_proto::worker::{
    self, DaemonMessage, Heartbeat, Hello, Phase, ServerMessage, Welcome, daemon_message,
    server_message, worker_client::WorkerClient,
};
use kbf_types::NodePhase;
use tokio::sync::{Notify, mpsc, watch};
use tokio::time::{sleep, timeout};
use tonic::transport::Endpoint;

use crate::clock::{Clock, SystemClock};
use crate::config::{ConfigError, DaemonConfig};
use crate::connect::{
    Backoff, Resolve, Server, SystemResolver, Target, WARN_EVERY, WarnLimit, resolve_all,
    rotate_after, unit_random,
};
use crate::core::{Event, LeaseCore, send};
use crate::drain::wait_for_drain;
use crate::extras::{ExtraWake, SessionExtras};
use crate::kbf_extras::KbfExtras;
use crate::lease::{Done, Leases, lease_id, proto_lease_id};
use crate::lease_minutes::usage_window;
use crate::report::NodeReport;
use crate::runtimes::Runtimes;
use crate::welcome;

/// The `kbf.worker.v1` protocol version this daemon speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// Why a session ended.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("connect: {0}")]
    Connect(#[from] tonic::transport::Error),
    #[error("client identity: {0}")]
    Identity(#[from] ConfigError),
    #[error("connect: no connection within {0:?}")]
    ConnectTimeout(Duration),
    #[error("stream: {0}")]
    Stream(#[from] tonic::Status),
    #[error("no Welcome within {0:?}")]
    WelcomeTimeout(Duration),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("no heartbeat acknowledged within {0:?}")]
    Silent(Duration),
}

impl SessionError {
    /// This error and every cause under it, outermost first, joined by `": "`: what
    /// the log says when a session ends (issue #170). A `tonic::transport::Error`
    /// displays as `transport error` alone; what failed (a refused connection, a TLS
    /// alert, a name that does not resolve) is only in its sources.
    ///
    /// Each variant's own text already includes the error it wraps, so the chain is
    /// followed from that error's source.
    #[must_use]
    pub fn with_causes(&self) -> String {
        let mut text = self.to_string();
        let wrapped = std::error::Error::source(self);
        let mut cause = wrapped.and_then(std::error::Error::source);
        while let Some(e) = cause {
            text.push_str(": ");
            text.push_str(&e.to_string());
            cause = e.source();
        }
        text
    }
}

/// A daemon: configuration, runtimes, node report and the state that outlives streams.
/// `X` answers the hooks of its session that name more than leases ([`SessionExtras`]);
/// the default is a kbf node's.
pub struct Daemon<X = KbfExtras> {
    config: DaemonConfig,
    /// The `--server` URLs, parsed.
    servers: Vec<Server>,
    /// How server names become addresses, asked again every round.
    resolver: Arc<dyn Resolve>,
    /// Whether the current (or the last) stream got its Welcome.
    welcomed: bool,
    /// The report the daemon was started with, with a `drivers` entry for each of its
    /// runtimes.
    base: NodeReport,
    /// What Hello carries and heartbeats hash: `base` plus the driver's newest entries.
    report: NodeReport,
    /// Whether the drain has begun, as [`Self::run_draining`] sets it; `None` for a
    /// daemon run by [`Self::run`], which never drains.
    drain: Option<watch::Receiver<bool>>,
    /// Notified when the daemon is draining and has nothing left to wait for.
    drained: Arc<Notify>,
    /// How many leases ran when the session loop last looked, for the drain's warning
    /// past its deadline (the stop future cannot reach `core`).
    running: Arc<AtomicUsize>,
    /// The lease state: leases, contact, Start window, unacknowledged Results, epoch.
    core: LeaseCore,
    /// Where the runs of `core`'s leases report their end.
    done: mpsc::UnboundedReceiver<Done>,
    /// What the node adds to the lease protocol.
    pub(crate) extras: X,
    /// This daemon process's `Hello.instance_id`, the same on every stream it opens.
    instance: String,
    /// `Hello.predecessor_instance_id`: the daemon process whose runs this one's drivers
    /// killed before it started; empty when none is known.
    predecessor: String,
}

/// What woke the session loop.
enum Wake {
    Message(Box<ServerMessage>),
    Beat,
    Done(Box<Done>),
    Recheck,
    Extra(ExtraWake),
    Drain,
}

impl Daemon<KbfExtras> {
    /// A daemon that will connect as `config` says. Reads the TLS files now, so a
    /// missing or unreadable file fails here rather than on the first connection, and
    /// detects the node's software ([`crate::status::Software::detect`]).
    ///
    /// A Start runs on the runtime of `runtimes` that serves its kind (one runtime, an
    /// `Arc<R>`, converts). The node report is `report` with a `drivers` entry for every
    /// runtime's driver added, whether `report` lists them or not.
    pub fn new(
        config: DaemonConfig,
        runtimes: impl Into<Runtimes>,
        report: NodeReport,
    ) -> Result<Self, ConfigError> {
        Self::with_extras(config, runtimes, report, KbfExtras::detect())
    }
}

impl<X: SessionExtras> Daemon<X> {
    /// [`Daemon::new`] for a node whose session hooks are `extras`.
    pub fn with_extras(
        config: DaemonConfig,
        runtimes: impl Into<Runtimes>,
        report: NodeReport,
        extras: X,
    ) -> Result<Self, ConfigError> {
        let runtimes = runtimes.into();
        let drivers = runtimes.drivers().into_iter().map(|d| ("drivers", d));
        let report = report.with_entries(drivers);
        let servers = config
            .servers
            .iter()
            .map(|url| Server::parse(url))
            .collect::<Result<Vec<_>, _>>()?;
        if servers.is_empty() {
            return Err(ConfigError::NoServer);
        }
        // Read once here so that unreadable files fail the start; every connection reads
        // them again (`endpoint`), so a certificate renewed while the daemon runs is the
        // one the next connection presents.
        config.tls.load()?;
        let (done_tx, done) = mpsc::unbounded_channel();
        let core = LeaseCore::new(
            Leases::new(runtimes, done_tx),
            config.fence_after,
            config.max_unacked,
            Arc::new(SystemClock),
        );
        Ok(Self {
            config,
            servers,
            resolver: Arc::new(SystemResolver),
            welcomed: false,
            base: report.clone(),
            report,
            drain: None,
            drained: Arc::new(Notify::new()),
            running: Arc::new(AtomicUsize::new(0)),
            core,
            done,
            extras,
            instance: instance_id(),
            predecessor: String::new(),
        })
    }

    /// This daemon process's instance id (`Hello.instance_id`), drawn when it was built.
    #[must_use]
    pub fn instance_id(&self) -> &str {
        &self.instance
    }

    /// Names `predecessor` in every Hello as the daemon process that ran on this node
    /// before this one (`Hello.predecessor_instance_id`). The caller must have killed
    /// every run that process left before this daemon runs: the server gives up that
    /// process's leases at once on the strength of it.
    #[must_use]
    pub fn with_predecessor(mut self, predecessor: String) -> Self {
        self.predecessor = predecessor;
        self
    }

    /// Sends every [`Event`] to `events` as well as to the log.
    #[must_use]
    pub fn with_events(mut self, events: mpsc::UnboundedSender<Event>) -> Self {
        self.core.observe(events);
        self
    }

    /// Takes the driver's newest report, if it sends any; returns whether the node
    /// report changed.
    pub(crate) fn take_driver(&mut self) -> bool {
        self.extras.take_driver(&self.base, &mut self.report)
    }

    /// Takes the runtime's newest phase, if it reports one; returns whether it changed.
    pub(crate) fn take_phase(&mut self) -> bool {
        self.extras.take_phase()
    }

    /// The phase every Hello carries.
    fn phase(&self) -> NodePhase {
        self.extras.hello_phase(self.core.draining)
    }

    /// Takes the newest pressure reading and the number of leases running; returns
    /// whether the phase a Hello carries changed.
    pub(crate) fn take_pressure(&mut self) -> bool {
        let leases = self.core.leases.running().len();
        self.extras.take_pressure(leases, self.core.draining)
    }

    /// Takes the drain's newest state; returns whether the phase a Hello carries
    /// changed.
    fn take_drain(&mut self) -> bool {
        let Some(drain) = &mut self.drain else {
            return false;
        };
        let began = *drain.borrow_and_update();
        let before = self.phase();
        self.core.draining = began;
        self.phase() != before
    }

    /// Wakes [`Self::run_draining`] when the daemon is draining and nothing is left to
    /// wait for: no lease runs and every Result is acknowledged.
    fn check_drained(&self) {
        self.running
            .store(self.core.leases.running().len(), Ordering::Relaxed);
        if self.core.drained() {
            self.drained.notify_one();
        }
    }

    /// Takes the drain, tells the server when the phase changed, and completes it when
    /// there is nothing left to wait for. A method of its own, not lines in the session
    /// loop (see [`Self::settle_drain`]).
    fn settle_drain(&mut self, tx: &UnboundedSender<DaemonMessage>) {
        if self.take_drain() {
            tracing::info!(phase = self.phase().name(), "node phase changed");
            send(tx, daemon_message::Message::Hello(self.hello()));
        }
        self.check_drained();
    }

    /// The `NodeStatus` this node sends now.
    #[must_use]
    pub fn node_status(&self) -> worker::NodeStatus {
        self.extras.node_status(&self.report)
    }

    /// Resolves server names with `resolver` instead of the operating system's. A test
    /// passes one whose answer it changes between rounds.
    #[must_use]
    pub fn with_resolver(mut self, resolver: Arc<dyn Resolve>) -> Self {
        self.resolver = resolver;
        self
    }

    /// Reads `clock` for the fence and the Start window instead of [`SystemClock`]. A
    /// test passes one it can jump forward, as a resume does.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.core.clock = clock;
        self
    }

    /// Runs the daemon until `shutdown` completes: connects, serves the session, and
    /// reconnects whenever the session ends, as [`crate::connect`] describes, for as
    /// long as it runs. v0 abandons running leases at shutdown; the scheduler
    /// re-dispatches them after G.
    pub async fn run(mut self, shutdown: impl Future<Output = ()>) {
        #[cfg(target_os = "linux")]
        crate::numa::log_layout();
        tokio::select! {
            () = self.reconnect_forever() => {}
            () = shutdown => tracing::info!("shutting down"),
        }
    }

    /// Runs the daemon like [`Self::run`], but drains on the first of `signals` (a
    /// `()` for each SIGTERM or SIGINT): see the module documentation. Returns when
    /// the drain is complete (no lease runs, no Result is unacknowledged) or at the
    /// second signal; `signals` closing does not stop it. `deadline`, if set, is when
    /// the drain warns that leases still run; it never ends the drain, so no lease is
    /// abandoned for it.
    pub async fn run_draining(
        mut self,
        mut signals: mpsc::UnboundedReceiver<()>,
        deadline: Option<Duration>,
    ) {
        #[cfg(target_os = "linux")]
        crate::numa::log_layout();
        let (begin, began) = watch::channel(false);
        self.drain = Some(began);
        let drained = Arc::clone(&self.drained);
        let running = Arc::clone(&self.running);
        let stop = async {
            // A closed channel is no signal.
            if signals.recv().await.is_none() {
                std::future::pending::<()>().await;
            }
            tracing::info!(?deadline, "draining: no new leases; running ones finish");
            begin.send_replace(true);
            wait_for_drain(&mut signals, &drained, deadline, &running).await;
        };
        tokio::select! {
            () = self.reconnect_forever() => {}
            () = stop => {}
        }
    }

    /// Rounds of connection attempts, without end: each round resolves every server
    /// again and tries each address in turn, starting after the last one tried; a
    /// round in which none was welcomed is followed by a growing wait.
    async fn reconnect_forever(&mut self) {
        let mut backoff = Backoff::new(self.config.reconnect_after, self.config.reconnect_max);
        let mut warn = WarnLimit::new(WARN_EVERY);
        let mut last = None;
        loop {
            let within = self.config.welcome_timeout;
            let (resolver, servers) = (Arc::clone(&self.resolver), self.servers.clone());
            let (targets, unresolved) =
                self.offline(resolve_all(&resolver, &servers, within)).await;
            for (server, reason) in unresolved {
                self.failed(&mut warn, server, &reason);
            }
            let mut welcomed = false;
            for target in rotate_after(targets, last) {
                last = Some(target.addr);
                let reason = match self.session(&target).await {
                    Ok(()) => "the server ended the stream".to_owned(),
                    Err(e) => e.with_causes(),
                };
                if self.welcomed {
                    tracing::warn!(server = %target, %reason, "session ended");
                    self.core.emit(Event::Disconnected(reason));
                    warn.welcomed();
                    welcomed = true;
                    break;
                }
                self.failed(&mut warn, target.to_string(), &reason);
            }
            let wait = if welcomed {
                backoff.welcomed(unit_random())
            } else {
                let wait = backoff.failed(unit_random());
                let failed_rounds = backoff.failed_rounds();
                tracing::debug!(failed_rounds, ?wait, "no server reached; waiting");
                self.core.emit(Event::Retrying {
                    failed_rounds,
                    wait,
                });
                wait
            };
            self.offline(sleep(wait)).await;
        }
    }

    /// Logs and reports an attempt that ended before Welcome: at WARN when `warn` allows,
    /// else at DEBUG.
    fn failed(&self, warn: &mut WarnLimit, server: String, reason: &str) {
        let (attempts, loud) = warn.failed(tokio::time::Instant::now());
        if loud {
            tracing::warn!(%server, %reason, attempts, "cannot reach a server; retrying");
        } else {
            tracing::debug!(%server, %reason, attempts, "cannot reach a server; retrying");
        }
        self.core.emit(Event::ConnectFailed {
            server,
            reason: reason.to_owned(),
        });
    }

    /// The endpoint for one attempt: `target`'s address, with the URL's host as the
    /// HTTP/2 authority and, unless `--tls-server-name` names another, as the name the
    /// server's certificate must carry; with the configured HTTP/2 keepalive. The
    /// client identity is read from its files for each attempt: a node's certificate is
    /// replaced every few days, and a connection made after that must present the new one
    /// (a stream already up keeps the certificate it was made with).
    fn endpoint(&self, target: &Target) -> Result<Endpoint, SessionError> {
        let mut tls = self.config.tls.load()?;
        if self.config.tls.server_name.is_none() {
            tls = tls.domain_name(target.server.host.clone());
        }
        let endpoint = Endpoint::from_shared(format!("https://{}", target.addr))?
            .origin(target.server.uri.clone())
            .tls_config(tls)?;
        Ok(self.config.keep_alive.apply(endpoint))
    }

    /// One stream to `target`, from connecting until it ends. Sets `welcomed` once the
    /// server's Welcome is taken.
    async fn session(&mut self, target: &Target) -> Result<(), SessionError> {
        self.welcomed = false;
        let endpoint = self.endpoint(target)?;
        let wait = self.config.welcome_timeout;
        let channel = self
            .offline(timeout(wait, endpoint.connect()))
            .await
            .map_err(|_| SessionError::ConnectTimeout(wait))??;
        let mut client = WorkerClient::new(channel);
        let (tx, rx) = unbounded();
        // A change that arrived while no stream was up.
        self.take_driver();
        self.take_phase();
        self.take_pressure();
        self.take_drain();
        let hello_sent = self.core.clock.now();
        send(&tx, daemon_message::Message::Hello(self.hello()));
        self.core.emit(Event::HelloSent);

        let wait = self.config.welcome_timeout;
        let response = self
            .offline(timeout(wait, client.session(rx)))
            .await
            .map_err(|_| SessionError::WelcomeTimeout(wait))??;
        let mut inbound = response.into_inner();
        let first = self
            .offline(timeout(wait, inbound.message()))
            .await
            .map_err(|_| SessionError::WelcomeTimeout(wait))??;
        let (interval, epoch, welcome) = self.welcome(first)?;
        self.welcomed = true;
        let usage_window = usage_window(interval, self.config.usage_window);
        self.core.new_stream(interval, usage_window, hello_sent);
        // Before the Welcome renews contact: a lease whose fence passed while this
        // stream was set up must not outlive it (`offline` checked already; this keeps
        // the order true by construction here too).
        self.core.recheck();
        // Before any Result is resent or any Start of this stream is read.
        self.new_epoch(epoch);
        let placement_load = welcome.placement_load == Some(true);
        self.extras.on_welcome(welcome);
        if self.core.contact.confirm(hello_sent) {
            self.core.emit(Event::ContactRestored);
        }
        tracing::info!(server = %target, ?interval, "welcomed");
        self.core.emit(Event::Welcomed {
            heartbeat_interval: interval,
        });
        // On every stream, so a reconnect to a server with another mode changes the limit
        // (absent is false); after contact is renewed, so a slow write cannot cost the
        // fence, and before the first Heartbeat of the stream, which reports what was
        // written.
        self.core.welcomed_placement_load(placement_load).await;
        // Before the first Heartbeat, which lists these leases (issue #26).
        for result in self.core.unacked.values() {
            send(&tx, daemon_message::Message::Result(result.clone()));
        }
        send(&tx, daemon_message::Message::NodeStatus(self.node_status()));

        // When this stream last acknowledged a heartbeat (or welcomed), on tokio's
        // clock, which stops during a suspend: a resumed daemon gives its stream T
        // awake to show it still works.
        let mut heard = tokio::time::Instant::now();
        let mut seq = 0u64;
        let mut beat = tokio::time::interval(interval);
        beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let wait = self.core.until_recheck(self.config.recheck_every);
            let wake = tokio::select! {
                msg = inbound.message() => match msg? {
                    Some(msg) => Wake::Message(Box::new(msg)),
                    None => return Ok(()),
                },
                _ = beat.tick() => Wake::Beat,
                Some(done) = self.done.recv() => Wake::Done(Box::new(done)),
                () = sleep(wait) => Wake::Recheck,
                extra = self.extras.wake() => Wake::Extra(extra),
                Some(()) = drain_changed(self.drain.as_mut()) => Wake::Drain,
            };
            // First, whatever woke the loop: the clock may have jumped over a suspend.
            self.core.recheck();
            self.check_silence(heard)?;
            match wake {
                Wake::Message(msg) => {
                    if let Some(server_message::Message::HeartbeatAck(_)) = msg.message {
                        heard = tokio::time::Instant::now();
                    }
                    self.on_message(*msg, &tx);
                }
                Wake::Beat => {
                    seq += 1;
                    let now = self.core.clock.now();
                    self.core.contact.sent(seq, now);
                    self.core.window.sent(seq, now);
                    let (pack_reads, pack_writes) = self.extras.pack_counts();
                    let node_load = self.core.node_load(seq).await;
                    let heartbeat = Heartbeat {
                        seq,
                        report_hash: self.report.hash().to_vec(),
                        running: self.core.listed().into_iter().map(proto_lease_id).collect(),
                        pack_reads,
                        pack_writes,
                        node_load,
                        input_cache: self.core.leases.runtimes.input_cache(),
                    };
                    send(&tx, daemon_message::Message::Heartbeat(heartbeat));
                }
                Wake::Done(done) => self.core.finished(Some(&tx), *done),
                Wake::Recheck => {}
                Wake::Extra(ExtraWake::Driver) => {
                    if self.take_driver() {
                        send(&tx, daemon_message::Message::Hello(self.hello()));
                    }
                    send(&tx, daemon_message::Message::NodeStatus(self.node_status()));
                }
                Wake::Extra(ExtraWake::Phase) => {
                    if self.take_phase() {
                        tracing::info!(phase = self.phase().name(), "node phase changed");
                        send(&tx, daemon_message::Message::Hello(self.hello()));
                    }
                }
                // The pressure is taken below, and the request for a map is sent by
                // `after_wake`, whatever woke the loop.
                Wake::Extra(ExtraWake::Pressure | ExtraWake::MapAsked) | Wake::Drain => {}
            }
            self.extras.after_wake(&tx);
            self.settle_drain(&tx);
            // Whatever woke the loop, leases may have started or ended.
            if self.take_pressure() {
                tracing::info!(phase = self.phase().name(), "node phase changed");
                send(&tx, daemon_message::Message::Hello(self.hello()));
            }
        }
    }

    /// Ends the session when contact is lost and this stream has acknowledged no
    /// heartbeat since `heard` for 1.5 T of time awake: it is stuck, or the server
    /// gave this node up, and a new stream says Hello again. Half T past the fence, so
    /// the Results of runs that stop at once go out on this stream first.
    fn check_silence(&self, heard: tokio::time::Instant) -> Result<(), SessionError> {
        let silent_for = self.core.contact.fence_after().saturating_mul(3) / 2;
        if self.core.contact.lost(self.core.clock.now()) && heard.elapsed() >= silent_for {
            return Err(SessionError::Silent(silent_for));
        }
        Ok(())
    }

    /// Checks the first server message is an acceptable Welcome; returns its interval,
    /// the lease epoch it names (`None` for 0) and the Welcome itself.
    fn welcome(
        &mut self,
        first: Option<ServerMessage>,
    ) -> Result<(Duration, Option<u64>, Welcome), SessionError> {
        let Some(server_message::Message::Welcome(w)) = first.and_then(|m| m.message) else {
            return Err(SessionError::Protocol(
                "the first server message is not Welcome".into(),
            ));
        };
        let oldest = welcome::OLDEST_PROTOCOL_VERSION;
        if !welcome::acceptable(
            w.protocol_version,
            oldest,
            PROTOCOL_VERSION,
            PROTOCOL_VERSION,
        ) {
            return Err(SessionError::Protocol(format!(
                "server speaks protocol version {}, this daemon {oldest} to {PROTOCOL_VERSION}",
                w.protocol_version
            )));
        }
        let interval = Duration::from_millis(w.heartbeat_interval_ms);
        // A gap is two intervals; it must be noticed well before the fence.
        if interval.is_zero() || interval.saturating_mul(2) >= self.core.contact.fence_after() {
            return Err(SessionError::Protocol(format!(
                "heartbeat interval {interval:?} does not fit the fence time {:?}",
                self.core.contact.fence_after()
            )));
        }
        self.extras.welcome_checked(&w);
        Ok((interval, (w.epoch != 0).then_some(w.epoch), w))
    }

    /// Takes the lease epoch a Welcome named: the node's hook first, then the core's
    /// (see [`LeaseCore::new_epoch`]).
    fn new_epoch(&mut self, epoch: Option<u64>) {
        change_epoch(&mut self.extras, &mut self.core, epoch);
    }

    fn on_message(&mut self, msg: ServerMessage, tx: &UnboundedSender<DaemonMessage>) {
        if let Some(message) = &msg.message
            && self.extras.on_server_message(message)
        {
            return;
        }
        match msg.message {
            Some(server_message::Message::HeartbeatAck(ack)) => self.core.on_heartbeat_ack(ack.seq),
            Some(server_message::Message::LeaseOffer(offer)) => {
                if let Some(id) = offer.lease_id.map(lease_id) {
                    tracing::info!(lease = %id, kind = %offer.kind, "lease offered");
                    self.core.emit(Event::Offered(id));
                }
            }
            Some(server_message::Message::Start(start)) => self.core.on_start(start, tx),
            Some(server_message::Message::ResultAck(ack)) => self.core.on_result_ack(&ack),
            Some(server_message::Message::Cancel(cancel)) => self.core.on_cancel(&cancel),
            Some(server_message::Message::Welcome(_)) => {
                tracing::warn!("a second Welcome on one stream ignored");
            }
            // A map is the node's (`on_server_message`); a node that takes none ignores it.
            // A frontend that still offers or orders an update is answered with nothing:
            // updates are installed from packages, not over this session.
            Some(
                server_message::Message::MapUpdate(_)
                | server_message::Message::UpdateOffer(_)
                | server_message::Message::UpdateOrder(_),
            ) => {}
            None => tracing::warn!("an empty server message ignored"),
        }
    }

    /// Runs `fut` while no stream is up, still finishing leases and fencing on time.
    /// The fence is checked after `fut` completes too, so what the caller does next
    /// (send a Hello, take a Welcome as contact) comes after any fence a resume made due.
    async fn offline<F: Future>(&mut self, fut: F) -> F::Output {
        let mut fut = std::pin::pin!(fut);
        loop {
            let wait = self.core.until_recheck(self.config.recheck_every);
            let (out, done) = tokio::select! {
                out = &mut fut => (Some(out), None),
                Some(done) = self.done.recv() => (None, Some(done)),
                () = sleep(wait) => (None, None),
                Some(()) = drain_changed(self.drain.as_mut()) => (None, None),
            };
            // First: the clock may have jumped over a suspend.
            self.core.recheck();
            if let Some(out) = out {
                return out;
            }
            if let Some(done) = done {
                self.core.finished(None, done);
            }
            self.take_drain();
            self.check_drained();
        }
    }

    fn hello(&self) -> Hello {
        Hello {
            protocol_version: PROTOCOL_VERSION,
            node_id: self.config.node_id.clone(),
            daemon_version: crate::DAEMON_VERSION.to_owned(),
            capabilities: self.report.capabilities().to_vec(),
            report_hash: self.report.hash().to_vec(),
            instance_id: self.instance.clone(),
            phase: wire_phase(&self.phase()),
            predecessor_instance_id: self.predecessor.clone(),
        }
    }
}

/// A new daemon process's instance id: 128 random bits as 32 hex digits, from two
/// hashers the standard library keys from the operating system's random source. It is
/// drawn at every start, so a restarted daemon, a second daemon started with the same
/// certificate and a daemon on a cloned machine each draw their own (issue #140). It is
/// written down for its successor only: `kbf-node` keeps it in the scratch directory,
/// and the next daemon names it as its predecessor ([`Daemon::with_predecessor`]).
fn instance_id() -> String {
    let half = || RandomState::new().hash_one(std::process::id());
    format!("{:016x}{:016x}", half(), half())
}

/// Completes when the drain has a state the daemon has not taken; never for a daemon
/// that does not drain, or once its sender is gone.
async fn drain_changed(drain: Option<&mut watch::Receiver<bool>>) -> Option<()> {
    drain?.changed().await.ok()
}

/// `phase` as Hello carries it: absent for serving, as a daemon that predates the
/// field sends it.
fn wire_phase(phase: &NodePhase) -> Option<worker::NodePhase> {
    let (state, lease, reason) = match phase {
        NodePhase::Serving => return None,
        NodePhase::Preparing(lease) => (Phase::Preparing, Some(*lease), ""),
        NodePhase::Restoring(lease) => (Phase::Restoring, Some(*lease), ""),
        NodePhase::Quarantined(reason) => (Phase::Quarantined, None, reason.as_str()),
        NodePhase::MemoryPressure => (Phase::MemoryPressure, None, ""),
        NodePhase::Draining => (Phase::Draining, None, ""),
    };
    Some(worker::NodePhase {
        phase: state.into(),
        lease_id: lease.map(proto_lease_id),
        reason: reason.to_owned(),
    })
}

/// A Welcome named lease epoch `epoch`: the node's hook first, then the core's change.
/// The hook goes first because it forgets leases the node holds outside the core (a
/// session switch's lease) by the epoch the core is about to replace; the core's change
/// then drops what it granted under the old one. Both kill by lease id and a kill of a
/// lease already killed does nothing, so what is dropped and what is reported does not
/// depend on which comes first, but the hook is written to see the core as the Welcome
/// found it.
fn change_epoch<X: SessionExtras>(extras: &mut X, core: &mut LeaseCore, epoch: Option<u64>) {
    extras.epoch_changed(epoch, core);
    core.new_epoch(epoch);
}

#[cfg(test)]
mod tests {
    use kbf_proto::reapi::Digest;
    use kbf_types::LeaseId;

    use crate::clock::SystemClock;
    use crate::runtime::FakeRuntime;

    use super::*;

    /// Catches a new epoch that leaves a lease which is the Mac session's switching lease
    /// and is also granted and running alive, or reports its Result: the run is killed
    /// (once, though the hook and the core both kill it), the Result of the run is
    /// dropped when it ends, nothing is kept for resending, and the observer is told the
    /// lease was superseded. Driven through [`change_epoch`], without a stream.
    #[tokio::test]
    async fn a_new_epoch_drops_a_switching_lease_that_runs_and_its_result() {
        let lease = LeaseId::new(1, 1);
        let runtime = Arc::new(FakeRuntime::new(Duration::from_secs(3600)));
        let (done_tx, mut done) = mpsc::unbounded_channel();
        let mut core = LeaseCore::new(
            Leases::new(Runtimes::new(Arc::clone(&runtime)), done_tx),
            Duration::from_secs(40),
            8,
            Arc::new(SystemClock),
        );
        let (events_tx, mut events) = mpsc::unbounded_channel();
        core.observe(events_tx);
        core.new_stream(
            Duration::from_secs(1),
            Duration::from_secs(1),
            core.clock.now(),
        );
        core.new_epoch(Some(1));
        let (tx, mut sent) = unbounded();
        let start = worker::Start {
            lease_id: Some(worker::LeaseId { term: 1, seq: 1 }),
            kind: "action".to_owned(),
            action_digest: Some(Digest::default()),
            ..worker::Start::default()
        };
        core.on_start(start, &tx);
        let mut extras = KbfExtras::detect();
        extras.phase_now = NodePhase::Preparing(lease);
        while runtime.started().is_empty() {
            tokio::task::yield_now().await;
        }

        change_epoch(&mut extras, &mut core, Some(2));
        let finished = done.recv().await.expect("the killed run ends");
        core.finished(Some(&tx), finished);

        assert_eq!(runtime.killed(), [lease]);
        assert_eq!(core.epoch, Some(2));
        assert!(core.unacked.is_empty(), "no Result kept");
        assert!(core.granted.is_empty(), "the grant is forgotten");
        assert!(sent.try_recv().is_err(), "no Result sent");
        assert_eq!(
            events.recv().await,
            Some(Event::Superseded(vec![lease])),
            "superseded first"
        );
        let ended = events
            .recv()
            .await
            .map(|e| matches!(e, Event::LeaseEnded(_)));
        assert_eq!(ended, Some(true), "then the run's end");
    }

    /// Catches (issue #170): a session error logged as `connect: transport error`
    /// alone, without the cause under it (here, the refused connection), and a chain
    /// that repeats the wrapped error.
    #[tokio::test]
    async fn a_connect_error_names_its_cause() {
        let port = kbf_testkit::port::dead_addr().port();
        let endpoint = Endpoint::from_shared(format!("http://127.0.0.1:{port}")).expect("url");
        let error = SessionError::from(endpoint.connect().await.expect_err("nothing listens"));
        let text = error.with_causes();
        assert!(text.starts_with("connect: transport error: "), "{text}");
        assert_eq!(text.matches("transport error").count(), 1, "{text}");
        assert!(text.to_lowercase().contains("refused"), "{text}");
        let plain = SessionError::Protocol("no Hello".to_owned());
        assert_eq!(plain.with_causes(), "protocol: no Hello");
    }
}
