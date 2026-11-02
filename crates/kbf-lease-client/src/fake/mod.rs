//! A lease server in the process, for tests of the code above [`Client`](crate::Client).
//!
//! [`FakeLeaseServer`] listens on a loopback port and speaks the real server's client
//! routes and bodies, with its admission rules: a lease books a whole host (the first free
//! one by id), the runs of a lease are admitted while `sum(limits) <= shape` of the pool, a
//! run's limits come back only when the run has `ended` (after it `exited`, as the host
//! stops listing it), a run's wall time is cut to what the lease has left, and the runs of
//! an earlier boot answer `410 run-forgotten`. What the hosts do is the test's to say, one
//! step at a time ([`FakeLeaseServer::run_started`], [`finish_run`](FakeLeaseServer::finish_run),
//! [`end_run`](FakeLeaseServer::end_run), [`lose_host`](FakeLeaseServer::lose_host),
//! [`restart`](FakeLeaseServer::restart)), so a test can hold a run between `exited` and
//! `ended`, which is where the real server's `409 no-capacity` comes from. Time is the
//! test's too ([`advance`](FakeLeaseServer::advance)); nothing here reads a clock.
//!
//! A [`Fault`] breaks the next request in a way the real server can: no answer, a dropped
//! connection, a short body, or a status of the test's choosing.
//!
//! The fake is checked against the same golden bodies as the client (`tests/golden`), and
//! against the client's own requests: the bodies it reads are the `deny_unknown_fields`
//! types of [`wire`](crate::AcquireRequest), as the server's are.

mod http;
mod state;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::config::{Config, Origin, Token};
use crate::ids::{LeaseId, RunId};
use crate::wire::{HostLoad, Mount, Outcome, RunFile, RunState};
use crate::{Client, ConfigError};

pub use state::Pool;
use state::State;

/// The bearer token the fake accepts.
pub const TOKEN: &str = "fake-lease-server-token-0123456789abcdef";

/// How the next request goes wrong.
#[derive(Debug, Clone)]
pub enum Fault {
    /// The connection is dropped without an answer.
    Close,
    /// The request is read and never answered.
    Hang,
    /// The answer starts well and the body ends short of its `content-length`.
    ShortBody,
    /// A status and body the test chooses.
    Reply {
        /// The HTTP status.
        status: u16,
        /// The body.
        body: Bytes,
    },
}

struct Shared {
    state: Mutex<State>,
    /// Each fault, with the prefix of the `METHOD /path` line of the request it breaks.
    faults: Mutex<VecDeque<(String, Fault)>>,
    requests: Mutex<Vec<String>>,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A lease server on a loopback port, until it drops.
pub struct FakeLeaseServer {
    shared: Arc<Shared>,
    addr: SocketAddr,
    tls: bool,
    task: JoinHandle<()>,
}

impl Drop for FakeLeaseServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeLeaseServer {
    /// Serves `pool` over plain HTTP on `127.0.0.1`.
    ///
    /// # Panics
    /// If no loopback port can be bound.
    pub async fn start(pool: Pool) -> Self {
        Self::serve(pool, None).await
    }

    /// Serves `pool` over TLS under `config`, on `127.0.0.1`. [`origin`](Self::origin) names
    /// the host `localhost`, so the certificate must carry that name.
    ///
    /// # Panics
    /// If no loopback port can be bound.
    pub async fn start_tls(pool: Pool, config: Arc<rustls::ServerConfig>) -> Self {
        Self::serve(pool, Some(tokio_rustls::TlsAcceptor::from(config))).await
    }

    async fn serve(pool: Pool, acceptor: Option<tokio_rustls::TlsAcceptor>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the fake lease server binds a loopback port");
        let addr = listener
            .local_addr()
            .expect("a bound listener has an address");
        let shared = Arc::new(Shared {
            state: Mutex::new(State::new(pool)),
            faults: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
        });
        let tls = acceptor.is_some();
        let task = tokio::spawn(http::accept_loop(listener, acceptor, Arc::clone(&shared)));
        Self {
            shared,
            addr,
            tls,
            task,
        }
    }

    /// The address clients dial.
    #[must_use]
    pub fn origin(&self) -> String {
        match self.tls {
            true => format!("https://localhost:{}", self.addr.port()),
            false => format!("http://{}", self.addr),
        }
    }

    /// A client for this server (plain HTTP only).
    ///
    /// # Errors
    /// The server was started with TLS, which needs the CA: build the client from
    /// [`config`](Self::config) and `with_ca_pem`.
    pub fn client(&self) -> Result<Client, ConfigError> {
        Client::new(self.config())
    }

    /// The config of a client of this server, without a CA.
    #[must_use]
    pub fn config(&self) -> Config {
        let origin: Origin = self
            .origin()
            .parse()
            .expect("the fake's origin is a valid origin");
        let token = Token::new(TOKEN).expect("the fake's token is a valid token");
        Config::new(origin, token)
    }

    // ---- what the world does

    /// Moves the clock on and expires the leases whose time is up.
    pub fn advance(&self, by: Duration) {
        let millis = u64::try_from(by.as_millis()).unwrap_or(u64::MAX);
        locked(&self.shared.state).advance(millis);
    }

    /// The server's clock, in milliseconds.
    #[must_use]
    pub fn now_ms(&self) -> u64 {
        locked(&self.shared.state).now_ms()
    }

    /// Restarts the server: leases persist, every run is forgotten, and every host is
    /// silent until [`host_hello`](Self::host_hello).
    pub fn restart(&self) {
        locked(&self.shared.state).restart();
    }

    /// The host says hello again (after a restart). False for a host the pool lacks.
    pub fn host_hello(&self, host: &str) -> bool {
        locked(&self.shared.state).host_hello(host)
    }

    /// The host of a pending lease is prepared. False if the lease is not pending.
    pub fn ready(&self, lease: LeaseId) -> bool {
        locked(&self.shared.state).ready(lease)
    }

    /// The host stops answering (`true`) or answers again (`false`). Its runs keep their
    /// last state.
    pub fn set_lost(&self, host: &str, lost: bool) -> bool {
        locked(&self.shared.state).set_lost(host, lost)
    }

    /// The host's agent reports `load` (`None`: its newest heartbeat carried none), which
    /// `GET /v1/hosts` lists unless the host is lost. False for a host the pool lacks.
    pub fn set_host_load(&self, host: &str, load: Option<HostLoad>) -> bool {
        locked(&self.shared.state).set_load(host, load)
    }

    /// The host reports a taint: it takes no new run.
    pub fn taint_host(&self, host: &str, reason: &str) -> bool {
        locked(&self.shared.state).taint(host, reason)
    }

    /// The host refuses every new run with `409` and `code` (`None`: takes runs again). It
    /// taints nothing, and the runs it has go on.
    pub fn refuse_posts(&self, host: &str, code: Option<&'static str>) -> bool {
        locked(&self.shared.state).refuse_posts(host, code)
    }

    /// The host starts a queued run. False if the run is not queued.
    pub fn run_started(&self, run: &RunId) -> bool {
        locked(&self.shared.state).run_started(run)
    }

    /// The host reports the run's result. The run is `exited` and still holds its limits.
    /// False if the run is not live.
    pub fn finish_run(
        &self,
        run: &RunId,
        outcome: Outcome,
        exit_code: Option<i32>,
        detail: Option<&str>,
    ) -> bool {
        locked(&self.shared.state).finish_run(run, outcome, exit_code, detail)
    }

    /// The host stops listing an `exited` run: it is `ended` and its limits come back.
    /// False if the run is not `exited`.
    pub fn end_run(&self, run: &RunId) -> bool {
        locked(&self.shared.state).end_run(run)
    }

    // ---- what the test looks at

    /// The wall limit the server gave the run, in milliseconds (cut to the lease's time
    /// left).
    #[must_use]
    pub fn run_wall_ms(&self, run: &RunId) -> Option<u64> {
        locked(&self.shared.state).run_wall_ms(run)
    }

    /// The scratch disk the run was queued with, as the server received it; `None` for a run
    /// that booked none and for a run the server does not know.
    #[must_use]
    pub fn run_disk_bytes(&self, run: &RunId) -> Option<u64> {
        locked(&self.shared.state).run_disk_bytes(run)
    }

    /// The arguments the run was queued with, as the server received them.
    #[must_use]
    pub fn run_args(&self, run: &RunId) -> Option<Vec<String>> {
        locked(&self.shared.state).run_args(run)
    }

    /// The image the run was queued with, as the server received it.
    #[must_use]
    pub fn run_image(&self, run: &RunId) -> Option<String> {
        locked(&self.shared.state).run_image(run)
    }

    /// The mounts the run was queued with, as the server received them.
    #[must_use]
    pub fn run_mounts(&self, run: &RunId) -> Option<Vec<Mount>> {
        locked(&self.shared.state).run_mounts(run)
    }

    /// The files the run was queued with, as the server received them. They are what the
    /// host would be sent; no route of the fake answers with them.
    #[must_use]
    pub fn run_files(&self, run: &RunId) -> Option<Vec<RunFile>> {
        locked(&self.shared.state).run_files(run)
    }

    /// The run's state, whoever is asking (a run of a forgotten boot is `None`).
    #[must_use]
    pub fn run_state(&self, run: &RunId) -> Option<RunState> {
        locked(&self.shared.state).run_state(run)
    }

    /// Breaks the next request that arrives (after the ones already queued).
    pub fn inject(&self, fault: Fault) {
        self.inject_on("", fault);
    }

    /// Breaks the next request whose `METHOD /path` line starts with `route`, such as
    /// `DELETE /v1/leases/`; requests before it are served as usual.
    pub fn inject_on(&self, route: &str, fault: Fault) {
        locked(&self.shared.faults).push_back((route.to_owned(), fault));
    }

    /// Every request seen so far, as `METHOD /path`, in arrival order. Empty for a server
    /// nothing called.
    #[must_use]
    pub fn requests(&self) -> Vec<String> {
        locked(&self.shared.requests).clone()
    }
}
