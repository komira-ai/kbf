//! The JSON bodies of the lease server's client routes, mirrored from the server
//! (`js-lease-server`: `wire.rs`, `run_api.rs`, `hosts.rs`).
//!
//! The bodies this client sends are the server's request types: exactly its fields, and
//! `deny_unknown_fields`, as the server has it, so a field added here that the server does
//! not know fails the golden tests instead of a 400 on the farm. The bodies it reads are
//! tolerant, as the formats rule asks of a reader: a field this version does not know is
//! skipped, and a state or outcome it does not know decodes to `Unknown` and leaves the rest
//! of the body readable. `tests/golden/*.json` pins every shape to the server's documented
//! bodies.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::ids::{LeaseId, RunId};

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

/// The resources a lease asks for. A dimension left out is zero.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    /// Thousandths of a CPU core.
    #[serde(default)]
    pub cpu_millis: u64,
    /// Bytes of memory.
    #[serde(default)]
    pub memory_bytes: u64,
    /// Whole GPUs.
    #[serde(default)]
    pub gpus: u64,
    /// The GPU class, named exactly when `gpus` is above zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_class: Option<String>,
}

/// The body of `POST /v1/leases`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcquireRequest {
    /// The pool's class.
    pub class: String,
    /// What the lease must hold.
    pub resources: Resources,
    /// Capabilities the host must have (`os`, `arch`, ...).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub capabilities: BTreeMap<String, String>,
    /// The most wall time the lease may last, whatever is renewed.
    pub max_wall_seconds: u64,
    /// Labels kept on the lease and listed back with it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// The validation run to stamp on the lease.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_run: Option<String>,
    /// How long the lease lasts unless renewed; the server's default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
}

impl AcquireRequest {
    /// A request for `resources` of `class` for at most `max_wall` (whole seconds).
    #[must_use]
    pub fn new(class: impl Into<String>, resources: Resources, max_wall: Duration) -> Self {
        Self {
            class: class.into(),
            resources,
            capabilities: BTreeMap::new(),
            max_wall_seconds: max_wall.as_secs(),
            labels: BTreeMap::new(),
            validation_run: None,
            ttl_seconds: None,
        }
    }

    /// Adds a label.
    #[must_use]
    pub fn label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.labels.insert(key.into(), value.into());
        self
    }

    /// Sets the lease's time to live between renewals (whole seconds).
    #[must_use]
    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl_seconds = Some(ttl.as_secs());
        self
    }
}

/// The state of a lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseState {
    /// Booked, and waiting for the provider to confirm the start.
    Pending,
    /// In force.
    Held,
    /// Being released; its runs are still stopping.
    Releasing,
    /// A state this version does not know.
    #[serde(other)]
    Unknown,
}

/// A lease that was granted: the 201 body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Granted {
    /// The lease.
    pub id: LeaseId,
    /// The host it holds.
    pub host: String,
    /// When it expires unless renewed, in milliseconds on the server's clock.
    pub expires_at_ms: u64,
}

/// A lease whose start the provider has not confirmed: the 202 body. It carries no host;
/// `GET /v1/leases` shows the lease `held` with its host once the provider confirms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingLease {
    /// The lease.
    pub id: LeaseId,
    /// `pending`.
    pub state: LeaseState,
}

/// The answer to an acquire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acquired {
    /// 201: the lease is held.
    Granted(Granted),
    /// 202: the lease is booked and waits for its host to be ready.
    Pending(PendingLease),
}

/// The 200 body of a renew.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Renewed {
    /// The new expiry, clamped to the ttl, the server's maximum ttl and the lease's
    /// maximum wall time.
    pub expires_at_ms: u64,
    /// The host is set aside for a waiting whole-node request. The renewal is not refused
    /// for it. Left out by the server when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub host_reserved: bool,
}

/// The body of `POST /v1/leases/{id}/renew`; empty (`{}`) asks for the default ttl.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RenewRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) ttl_seconds: Option<u64>,
}

/// How far a release got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseState {
    /// 200: the host is free.
    Freed,
    /// 202: the lease's runs are being stopped; the host is free when they end.
    Releasing,
    /// A state this version does not know.
    #[serde(other)]
    Unknown,
}

/// The body of a release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Released {
    /// How far it got.
    pub state: ReleaseState,
}

/// One lease of `GET /v1/leases`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    /// The lease.
    pub id: LeaseId,
    /// Its state.
    pub state: LeaseState,
    /// The host it holds, once it holds one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// When it was acquired, in milliseconds on the server's clock.
    pub acquired_at_ms: u64,
    /// When it expires unless renewed.
    pub expires_at_ms: u64,
    /// When it ends whatever is renewed.
    pub ends_at_ms: u64,
    /// The labels it was acquired with.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

/// The body of `GET /v1/leases`.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct LeaseList {
    pub(crate) leases: Vec<Lease>,
}

/// What a host holds: the shape of a lease on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shape {
    /// Thousandths of a CPU core.
    pub cpu_millis: u64,
    /// Bytes of memory.
    pub memory_bytes: u64,
    /// Whole GPUs.
    pub gpus: u64,
    /// The GPU class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_class: Option<String>,
    /// Bytes of scratch disk a host books runs against; absent when the server reports none
    /// (and then it refuses no run for its disk).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_bytes: Option<u64>,
}

/// Where a host comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostSource {
    /// The operator's pool file.
    Static,
    /// A host that said hello.
    Registered,
    /// A source this version does not know.
    #[serde(other)]
    Unknown,
}

/// A registered host's liveness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostState {
    /// Heard from lately.
    Live,
    /// Silent for longer than its grace.
    Lost,
    /// Known, and waiting to say hello again (after a server restart).
    Awaiting,
    /// A state this version does not know.
    #[serde(other)]
    Unknown,
}

/// A host's report that it can no longer be trusted clean.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Taint {
    /// A short code for why.
    pub reason: String,
    /// The run whose job or cleanup led to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// The check that found it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
}

/// One host of `GET /v1/hosts`. The fields after `labels` are those of a registered host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
    /// The host.
    pub id: String,
    /// Where it comes from.
    pub source: HostSource,
    /// What a lease on it holds.
    pub shape: Shape,
    /// Its capabilities.
    #[serde(default)]
    pub capabilities: BTreeMap<String, String>,
    /// Its labels.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// The lease holding it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<LeaseId>,
    /// Its liveness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<HostState>,
    /// Whether it is lost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lost: Option<bool>,
    /// The phase its agent reported (`serving`, `draining`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// The agent's version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_version: Option<String>,
    /// It holds no lease but has runs to finish, so it takes no new lease yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub releasing: Option<bool>,
    /// It reported a taint: it is sent no start and takes no new run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taint: Option<Taint>,
    /// It is set aside: `reserved`, `held`, `draining` or `releasing`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mark: Option<String>,
    /// The request a reserved host is set aside for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserved_for: Option<String>,
    /// When a reservation that is draining a busy host is given up, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drain_deadline_ms: Option<u64>,
    /// The latest load its agent reported, if its newest heartbeat carried one. Absent for
    /// a host that has no reading, and for a server that does not send the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load: Option<HostLoad>,
}

/// The latest reading of a host's actions control group, as `GET /v1/hosts` lists it: what
/// the agent measured, in the units the field names state. CPU time is a counter, so a rate
/// is the difference of two readings over the difference of their `mono_usec`. A response
/// type: a field a newer server adds is ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostLoad {
    /// The reading's number within the agent's process.
    pub seq: u64,
    /// The agent's monotonic clock at the reading, in microseconds.
    pub mono_usec: u64,
    /// Cumulative CPU time of the cgroup that holds every action, in microseconds.
    pub cpu_usage_usec: u64,
    /// Memory in use by the actions, in bytes.
    pub memory_working_set_bytes: u64,
    /// Active file cache, in bytes.
    pub memory_active_file_bytes: u64,
    /// Whether the actions cgroup has a CPU limit.
    pub actions_cpu_limited: bool,
    /// Leases (`term-seq`) that ran the whole window since the previous reading.
    #[serde(default)]
    pub covered: Vec<String>,
    /// When each running lease started, on the agent's clock.
    #[serde(default)]
    pub lease_clocks: Vec<HostLeaseClock>,
    /// Closed lease-minutes.
    #[serde(default)]
    pub minutes: Vec<HostLeaseMinute>,
}

/// When one running lease started, on the agent's clock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostLeaseClock {
    /// The lease, `term-seq`.
    pub lease: String,
    /// Microseconds on the agent's monotonic clock.
    pub started_mono_usec: u64,
}

/// What one lease used in one minute of its life.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostLeaseMinute {
    /// The lease, `term-seq`.
    pub lease: String,
    /// The minute since the lease's start.
    pub minute: u32,
    /// Mean CPU over the observed span, in thousandths of a core.
    pub cpu_mean_millis: u32,
    /// The highest CPU rate of any window in the minute, in thousandths of a core.
    pub cpu_peak_window_millis: u32,
    /// The largest working set seen in the minute, in bytes.
    pub ws_max_bytes: u64,
}

impl Host {
    /// Whether the host stopped answering.
    #[must_use]
    pub fn is_lost(&self) -> bool {
        self.lost == Some(true) || self.state == Some(HostState::Lost)
    }

    /// Whether the host reported a taint.
    #[must_use]
    pub fn is_tainted(&self) -> bool {
        self.taint.is_some()
    }
}

/// The body of `GET /v1/hosts`.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct HostList {
    pub(crate) hosts: Vec<Host>,
}

/// A host directory made visible to a run: a path under a root the agent's own
/// configuration names, never a path from the message alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    /// The name of one of the agent's roots (`--mount-root NAME=PATH`).
    pub root_name: String,
    /// The path below that root: relative, no `..`, no `:` or `,`.
    pub rel: String,
    /// Whether the run sees it read-only.
    pub ro: bool,
}

/// Whether a run may change the machine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    /// A container with the stated limits and mounts.
    #[default]
    Contained,
    /// The run may change the machine. The server refuses it on this API.
    Host,
}

/// The limits of a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunLimits {
    /// Thousandths of a CPU core.
    pub cpu_millis: u64,
    /// Bytes of memory (the swap limit is the same).
    pub memory_bytes: u64,
    /// The most processes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids: Option<u64>,
    /// The wall time in milliseconds. The server cuts it to the lease's time left, and
    /// uses that time when it is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_ms: Option<u64>,
    /// Bytes of scratch disk the run books on its host (inputs, outputs and the private
    /// home, tmp and cache). Absent books none; a server that does not know the field is not
    /// sent it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_bytes: Option<u64>,
}

/// One file the agent writes for the run before it starts: a per-run secret such as a
/// presigned URL or a token, which the run reads at `/run/js-files/<name>` and which no
/// argument or label should carry.
///
/// The lease server refuses a bad file with `400 invalid-file` and a host that did not report
/// the `run-files` capability with `409 files-unsupported`. The limits are the server's: at
/// most 8 files; a name of `[A-Za-z0-9._-]`, leading alphanumeric, at most 64 bytes and
/// unique ignoring case; 16 KiB of contents a file and 64 KiB in all; no NUL.
///
/// The contents are secret: `Debug` prints the name and the length of the contents and
/// nothing else, so no log line that prints a spec can leak them.
///
/// Who owns the secret's lifetime: the contents are a plain `String`, copied into the encoded
/// body, and none of it is zeroized. This crate writes them nowhere (no file, no log) and
/// keeps them only as long as the caller keeps the spec. The caller decides when they are
/// dropped.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFile {
    /// The file name.
    pub name: String,
    /// The contents, UTF-8 text.
    pub contents: String,
}

impl fmt::Debug for RunFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunFile")
            .field("name", &self.name)
            .field("contents_len", &self.contents.len())
            .finish_non_exhaustive()
    }
}

/// The body of `POST /v1/leases/{id}/runs`.
///
/// `Debug` prints the number of arguments and of files and never their contents: they may
/// carry a credential (a presigned URL), and this type is what a log line would print.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunSpec {
    /// The image, pinned by digest (`name@sha256:<64 hex>`).
    pub image: String,
    /// The arguments (at most 1024, 16 KiB each, 256 KiB together).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// The limits.
    pub limits: RunLimits,
    /// The GPU indices, one run each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpu_indices: Vec<u32>,
    /// The mounts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<Mount>,
    /// Secret files the run reads at `/run/js-files/<name>`. Absent from the body when
    /// empty, so a start without files is the bytes it has always been.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<RunFile>,
    /// Whether the run may change the machine.
    #[serde(default, skip_serializing_if = "is_default")]
    pub isolation: Isolation,
    /// Container labels (not secret-safe).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

impl RunSpec {
    /// A run of `image` under `limits`, with no arguments, mounts, files or labels.
    #[must_use]
    pub fn new(image: impl Into<String>, limits: RunLimits) -> Self {
        Self {
            image: image.into(),
            args: Vec::new(),
            limits,
            gpu_indices: Vec::new(),
            mounts: Vec::new(),
            files: Vec::new(),
            isolation: Isolation::default(),
            labels: BTreeMap::new(),
        }
    }
}

impl fmt::Debug for RunSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunSpec")
            .field("image", &self.image)
            .field("args", &format_args!("<{} redacted>", self.args.len()))
            .field("limits", &self.limits)
            .field("gpu_indices", &self.gpu_indices)
            .field("mounts", &self.mounts)
            .field("files", &format_args!("<{} redacted>", self.files.len()))
            .field("isolation", &self.isolation)
            .field("labels", &self.labels)
            .finish()
    }
}

/// The state of a run, as the lease server tracks it. The server is told the run's state by
/// the host's heartbeat, so it trails the host by one or two beats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunState {
    /// Accepted, and not yet sent to the host.
    Queued,
    /// Sent to the host.
    Starting,
    /// Listed by the host as running.
    Running,
    /// The host reported a result and still lists the run. Its limits are still booked.
    Exited,
    /// A stop was asked for and the host still lists the run.
    Stopping,
    /// The host no longer lists the run; its limits are free again.
    Ended,
    /// The host was lost before it listed the run, or the run was stopped while queued.
    NeverStarted,
    /// A state this version does not know.
    #[serde(other)]
    Unknown,
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The job exited with status zero.
    Succeeded,
    /// The job exited with a nonzero status.
    Failed,
    /// The job was stopped or cancelled.
    Killed,
    /// The job passed its wall-time limit and was killed.
    TimedOut,
    /// The job passed its memory limit.
    OutOfMemory,
    /// No container could be started; no job ran.
    FailedToStart,
    /// An outcome this version does not know.
    #[serde(other)]
    Unknown,
}

/// A run, as `GET .../runs/{run_id}` and the run list show it. The result carries the
/// outcome and exit code only: the server keeps no output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    /// The run.
    pub run_id: RunId,
    /// Its state.
    pub state: RunState,
    /// How it ended, once the host reported a result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
    /// The job's exit status, when it exited itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// A refusal code or a short text (at most 1024 bytes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The labels the run was queued with, when the server lists them (older servers do
    /// not). They let a caller find a run whose 202 it never saw.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

/// The 202 body of a queued or stopped run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunAccepted {
    /// The run; the server mints it.
    pub run_id: RunId,
    /// Its state: `queued` for a new run.
    pub state: RunState,
}

/// The body of `GET /v1/leases/{id}/runs`.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RunList {
    pub(crate) runs: Vec<Run>,
}

/// An error body: `{"error": "<code>"}`, with a `reason` for a 400.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ErrorBody {
    pub(crate) error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
}
