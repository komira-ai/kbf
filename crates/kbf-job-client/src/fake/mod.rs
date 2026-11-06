//! A job service in the process, for tests of the code above [`Client`](crate::Client).
//!
//! [`FakeJobServer`] listens on a loopback port and serves the generated `JobService` of
//! `js-proto` with the service's rules (design sections 3.2 and 5.3):
//!
//! - **Submit is idempotent on the job id.** The same id with the same job (every field but
//!   the priority) is answered from the job it made, in whatever state it is now; the same id
//!   with a different job is `ALREADY_EXISTS` with the `job-id-conflict` refusal. An id is
//!   free again once the retention window of the job that held it has passed. A job with no
//!   id gets one (`j-<counter>`).
//! - **Admission.** A job that no host ([`add_host`](FakeJobServer::add_host)) and no offer
//!   ([`add_offer`](FakeJobServer::add_offer)) could ever run is refused `no-capable-host`
//!   and nothing is recorded; a wall limit that is missing or 0 is `wall-time-not-positive`,
//!   one above the ceiling ([`set_wall_ceiling`](FakeJobServer::set_wall_ceiling))
//!   `wall-time-above-ceiling`. An id the service would not take, or an image not pinned by
//!   digest, is `INVALID_ARGUMENT` without a refusal.
//! - **States are the test's to move**, one step at a time: [`place`](FakeJobServer::place),
//!   [`starting`](FakeJobServer::starting), [`run`](FakeJobServer::run),
//!   [`finish`](FakeJobServer::finish), [`fail_to_start`](FakeJobServer::fail_to_start),
//!   [`lose`](FakeJobServer::lose), [`host_stopped`](FakeJobServer::host_stopped). Cancel of a
//!   queued job cancels it at once and marks it `cancel_requested`, as the released service
//!   does; of a placed, starting or running job it records the intent and the job is cancelled
//!   only at `host_stopped`; of an ended job it changes nothing.
//! - **Results and logs are kept for the retention window** after a job ends, on the fake's
//!   own clock ([`advance`](FakeJobServer::advance); nothing here reads a clock), seven days
//!   unless [`set_retention`](FakeJobServer::set_retention) says otherwise. A job past it is
//!   `NOT_FOUND` to every call. `GetResult` of a live job is `FAILED_PRECONDITION`; `GetLogs`
//!   of a running job answers the tail so far ([`write_logs`](FakeJobServer::write_logs)), cut
//!   to the last 8 KiB of each stream.
//! - **Host silence shows on `GetJob`** ([`silence_host`](FakeJobServer::silence_host)) on
//!   the placed, starting and running jobs of the host, and changes no state.
//! - **A network level left unspecified is `none`**, as the released service records it: a
//!   job submitted without one is answered with `NETWORK_LEVEL_NONE`, and the two are the same
//!   job to a repeated submit.
//! - **File contents are never returned**: every job a call answers has them empty.
//! - **The bearer token ([`TOKEN`]) is required** on every call, else `UNAUTHENTICATED`.
//!
//! A [`Fault`] breaks the next call in a way a real service or network can.
//! [`calls`](FakeJobServer::calls) is every request the service handled, decoded; no
//! authorization value is in it.

mod http;
mod service;
mod state;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use js_proto::v1::{
    CancelJobRequest, GetJobRequest, GetLogsRequest, GetResultRequest, Job, JobSpec,
    ListJobsRequest, StartFailureCode, SubmitJobRequest,
};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tonic::{Code, Status};

use crate::config::{Config, Token};
use crate::{Client, ConfigError};

pub use state::Machine;
use state::State;

/// The bearer token the fake accepts.
pub const TOKEN: &str = "fake-job-server-token-0123456789abcdef";

/// How the next call goes wrong.
#[derive(Debug, Clone)]
pub enum Fault {
    /// The call is applied, then the connection is closed before the answer: the server has
    /// acted and the client has no answer.
    LoseAnswer,
    /// The call is read and logged, never answered, and not applied.
    /// [`FakeJobServer::hung`] tells when a call has taken it.
    Hang,
    /// The call is read and logged, answered with this status without details, and not
    /// applied.
    Status {
        /// The status's code.
        code: Code,
        /// The status's message.
        message: String,
    },
    /// The call is applied and answered OK with these bytes as its message.
    Answer(Bytes),
    /// The call is applied and answered with `body` as the whole response body, as it is
    /// (well-formed gRPC frames or not), then trailers of status `code`.
    Raw {
        /// The response body.
        body: Bytes,
        /// The `grpc-status` of the trailers.
        code: Code,
    },
}

/// One call the service handled, with its request as decoded.
#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    /// `SubmitJob`.
    SubmitJob(Box<SubmitJobRequest>),
    /// `GetJob`.
    GetJob(GetJobRequest),
    /// `ListJobs`.
    ListJobs(ListJobsRequest),
    /// `CancelJob`.
    CancelJob(CancelJobRequest),
    /// `GetResult`.
    GetResult(GetResultRequest),
    /// `GetLogs`.
    GetLogs(GetLogsRequest),
}

struct Shared {
    state: Mutex<State>,
    /// Each fault, with the method it breaks (empty: any).
    faults: Mutex<VecDeque<(String, Fault)>>,
    calls: Mutex<Vec<Call>>,
    /// Told each time a call takes a [`Fault::Hang`].
    hung: tokio::sync::Notify,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        locked(&self.state)
    }

    fn record(&self, call: Call) {
        locked(&self.calls).push(call);
    }

    /// The first fault queued for `method` or for any method, taken off the queue.
    fn fault_for(&self, method: &str) -> Option<Fault> {
        let mut faults = locked(&self.faults);
        let at = faults
            .iter()
            .position(|(on, _)| on.is_empty() || on == method)?;
        faults.remove(at).map(|(_, fault)| fault)
    }
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A job service on a loopback port, until it drops.
pub struct FakeJobServer {
    shared: Arc<Shared>,
    addr: SocketAddr,
    tls: bool,
    task: JoinHandle<()>,
}

impl Drop for FakeJobServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl FakeJobServer {
    /// Serves over plain HTTP/2 on a port of `127.0.0.1` the system picks.
    ///
    /// # Errors
    /// No loopback port can be bound.
    pub async fn start() -> std::io::Result<Self> {
        Self::serve(None).await
    }

    /// Serves over TLS under `config` on a port of `127.0.0.1`; [`origin`](Self::origin)
    /// names the host `localhost`, so the certificate must carry that name. The fake offers
    /// `h2` as the TLS protocol, whatever `config` says.
    ///
    /// # Errors
    /// No loopback port can be bound.
    pub async fn start_tls(config: Arc<rustls::ServerConfig>) -> std::io::Result<Self> {
        let mut config = (*config).clone();
        config.alpn_protocols = vec![b"h2".to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        Self::serve(Some(acceptor)).await
    }

    async fn serve(acceptor: Option<tokio_rustls::TlsAcceptor>) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State::new()),
            faults: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
            hung: tokio::sync::Notify::new(),
        });
        let tls = acceptor.is_some();
        let task = tokio::spawn(http::accept_loop(listener, acceptor, Arc::clone(&shared)));
        Ok(Self {
            shared,
            addr,
            tls,
            task,
        })
    }

    /// The address clients dial.
    #[must_use]
    pub fn origin(&self) -> String {
        match self.tls {
            true => format!("https://localhost:{}", self.addr.port()),
            false => format!("http://{}", self.addr),
        }
    }

    /// The config of a client of this server, with its token and without a CA.
    ///
    /// # Errors
    /// None in practice: the origin and the token are the fake's own.
    pub fn config(&self) -> Result<Config, ConfigError> {
        Ok(Config::new(self.origin().parse()?, Token::new(TOKEN)?))
    }

    /// A client of this server (plain HTTP only).
    ///
    /// # Errors
    /// The server was started with TLS, which needs the CA: build the client from
    /// [`config`](Self::config) and `with_ca_pem`.
    pub fn client(&self) -> Result<Client, ConfigError> {
        Client::new(self.config()?)
    }

    // ---- the clock and the settings

    /// Moves the clock on; jobs whose retention window has passed are forgotten.
    pub fn advance(&self, by: Duration) {
        self.shared.state().advance(millis(by));
    }

    /// The fake's clock, in milliseconds; it starts at 0.
    #[must_use]
    pub fn now_ms(&self) -> u64 {
        self.shared.state().now_ms()
    }

    /// How long a job is kept after it ends (seven days until this is called).
    pub fn set_retention(&self, window: Duration) {
        self.shared.state().set_retention_ms(millis(window));
    }

    /// The most wall limit a job may ask for (one day until this is called).
    pub fn set_wall_ceiling(&self, ceiling: Duration) {
        self.shared.state().set_wall_ceiling_s(ceiling.as_secs());
    }

    // ---- what the world has

    /// Adds a host, which jobs can be placed on and which admits the jobs it fits.
    pub fn add_host(&self, id: &str, machine: Machine) {
        self.shared.state().add_host(id, machine);
    }

    /// Adds a machine a provider could launch: it admits the jobs it fits, and holds none.
    pub fn add_offer(&self, machine: Machine) {
        self.shared.state().add_offer(machine);
    }

    /// The host misses its grace from now: its placed, starting and running jobs show
    /// `host_silent_since_ms`, and keep their state. False for no such host.
    pub fn silence_host(&self, host: &str) -> bool {
        self.shared.state().silence_host(host, true)
    }

    /// The host is heard again. False for no such host.
    pub fn unsilence_host(&self, host: &str) -> bool {
        self.shared.state().silence_host(host, false)
    }

    // ---- what the hosts do (each false when the job is not in the state the step needs)

    /// A queued job is placed on a host the fake has.
    pub fn place(&self, job: &str, host: &str) -> bool {
        self.shared.state().place(job, host)
    }

    /// A placed job is starting: its host's agent has the start.
    pub fn starting(&self, job: &str) -> bool {
        self.shared.state().start(job)
    }

    /// A starting job is running.
    pub fn run(&self, job: &str) -> bool {
        self.shared.state().run(job)
    }

    /// A running job writes to its standard output and error.
    pub fn write_logs(&self, job: &str, stdout: &[u8], stderr: &[u8]) -> bool {
        self.shared.state().write_logs(job, stdout, stderr)
    }

    /// A running job exits by itself with `exit_code`: succeeded for 0, failed otherwise.
    pub fn finish(&self, job: &str, exit_code: i32) -> bool {
        self.shared.state().finish(job, exit_code)
    }

    /// A queued, placed or starting job fails to start with `code`.
    pub fn fail_to_start(&self, job: &str, code: StartFailureCode) -> bool {
        self.shared.state().fail_to_start(job, code)
    }

    /// A job on a host is lost (the provider reads the host absent, or the host came back
    /// without it).
    pub fn lose(&self, job: &str) -> bool {
        self.shared.state().lose(job)
    }

    /// The host reports stopped a job whose cancel was asked for: it is cancelled.
    pub fn host_stopped(&self, job: &str) -> bool {
        self.shared.state().host_stopped(job)
    }

    // ---- what the test looks at

    /// Every job the fake keeps, as a read answers it (file contents empty), in the order
    /// they were recorded.
    #[must_use]
    pub fn jobs(&self) -> Vec<Job> {
        self.shared.state().jobs()
    }

    /// The job `id` as it was submitted, file contents included: what its host would be
    /// sent. No call of the service answers with it.
    #[must_use]
    pub fn submitted(&self, id: &str) -> Option<JobSpec> {
        self.shared.state().submitted(id)
    }

    /// Records a job as `SubmitJob` would, without a call and without an entry in
    /// [`calls`](Self::calls): for a test that needs many jobs.
    ///
    /// # Errors
    /// The status `SubmitJob` would answer.
    pub fn record(&self, spec: JobSpec) -> Result<Job, Status> {
        self.shared.state().submit(spec)
    }

    /// Breaks the next call that arrives (after the faults already queued).
    pub fn inject(&self, fault: Fault) {
        self.inject_on("", fault);
    }

    /// Breaks the next call of `method` (such as `SubmitJob`); other calls are served as
    /// usual.
    pub fn inject_on(&self, method: &str, fault: Fault) {
        locked(&self.shared.faults).push_back((method.to_owned(), fault));
    }

    /// Waits until a call has taken a [`Fault::Hang`] (at once if one has since the last
    /// wait): the request has reached the fake and will never be answered.
    pub async fn hung(&self) {
        self.shared.hung.notified().await;
    }

    /// The next call is applied and its answer lost: [`Fault::LoseAnswer`].
    pub fn lose_next_answer(&self) {
        self.inject(Fault::LoseAnswer);
    }

    /// Every call the service read, in arrival order, with its request as decoded: the calls
    /// it handled and those a [`Fault`] answered without applying them. A call refused for its
    /// token is not here.
    #[must_use]
    pub fn calls(&self) -> Vec<Call> {
        locked(&self.shared.calls).clone()
    }
}
