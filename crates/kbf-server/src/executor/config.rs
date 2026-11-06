//! What the executor is configured with, and what follows from it: the node report of a
//! virtual worker, the needs and the constraints of a job, the wall limit of a job, and the
//! timers, which are checked against the scheduler's own.

use std::time::Duration;

use js_proto::v1::{Constraints, ExecMode, NetworkLevel};
use kbf_proto::worker::{Capability, Hello};
use kbf_sched::fence::{LEADER_LEASE_MARGIN, LEASE_GRACE, SELF_FENCE, START_GRACE, START_VALIDITY};
use ring::rand::{SecureRandom, SystemRandom};

use crate::config::MAX_HEARTBEAT_INTERVAL_MS;
use crate::executor::task::Delivery;
use crate::worker::PROTOCOL_VERSION;

const GIB: u64 = 1 << 30;

/// The longest a presigned url may be asked to stay good. The store refuses more, and the
/// executor does not ask: [`Config::presign_ceiling`] stays under it, and the longest job's
/// wall limit under that.
pub const PRESIGN_LIMIT: Duration = Duration::from_secs(4 * 3600);

/// What the container driver adds to 1.5 times a booking for the hard memory cap of a run
/// (`kbf-driver-container`, `cgroup.rs`). A job needs the memory it is sent and its host caps
/// it there, so the executor sends the same number and an out-of-memory job is the same event
/// as an out-of-memory run on a daemon.
const HARD_CAP_PAD: u64 = 512 << 20;

/// What a job needs for its outputs when the operator says nothing: 8 GiB. A host serves its
/// scratch to all its jobs, so this times the jobs a host runs at once (and the margin, and the
/// inputs) is what a full host asks of it.
pub const DEFAULT_OUTPUT_DISK: u64 = 8 * GIB;

/// What a job needs beyond its inputs and its output allowance: 1 GiB for what neither counts
/// (the task tool's own files, the manifest and logs, the filesystem's rounding).
pub const DISK_MARGIN: u64 = GIB;

/// The network a job of the executor is given on its host (`--executor-network`): the
/// job's `network` level, which the job service passes to the host's agent. A job that asks
/// for none is given none, so every job names one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Network {
    /// No route out: the task can reach neither the read listener nor the store, so every
    /// action fails. For a farm whose agents give jobs no network yet.
    None,
    /// The endpoints the host's agent allows, and nothing else: the read listener and the
    /// store, when the agent is set up for them. The default.
    #[default]
    AllowList,
    /// Any address but those the agent always denies. Only with
    /// [`Config::allow_open_network`].
    Open,
}

impl Network {
    /// The job's `network` level: never unspecified, which the service reads as none.
    #[must_use]
    pub fn level(self) -> NetworkLevel {
        match self {
            Self::None => NetworkLevel::None,
            Self::AllowList => NetworkLevel::AllowList,
            Self::Open => NetworkLevel::Open,
        }
    }
}

/// The executor's settings. Every duration is an upper or lower bound the executor holds
/// itself to; [`Config::validate`] refuses a set that breaks the ones the scheduler's
/// safety rests on.
#[derive(Clone, Debug)]
pub struct Config {
    /// This frontend's id: the value of the `kbf-frontend` label on its jobs, hashed into
    /// their ids, and the name of its virtual workers (`js:<frontend>-<n>`).
    pub frontend: String,
    /// Which process of this frontend id this is: the value of the `kbf-instance` label on its
    /// jobs, made up at start. A job of another process under the same frontend id is cancelled
    /// at start only once that process is seen to make no new job ([`Config::adopt_watch`]).
    pub instance: String,
    /// How many virtual workers the farm places on, each of the shape below.
    pub hosts: usize,
    /// What a virtual worker holds in CPU, in thousandths of a core: the most the farm books
    /// on it at once.
    pub cpu_millis: u64,
    /// What a virtual worker holds in memory, in bytes: the bookings on it and their hard
    /// caps fit in it ([`Config::booked_memory_gib`]).
    pub memory_bytes: u64,
    /// The most actions a virtual worker has in flight (`action_slots` of its node report).
    pub slots: u64,
    /// The CPU architecture a job asks its host for.
    pub arch: String,
    /// The longest action timeout the executor runs.
    pub max_run: Duration,
    /// What a job's wall limit adds to the action's timeout: fetch, pack and upload. The task
    /// tool's own deadline bounds only the action's command, so a fetch of a large input tree
    /// or an upload that outlasts this margin ends the job at its wall limit, and is answered
    /// to the caller as a timeout and not rerun: raise it for slower stores.
    pub run_margin: Duration,
    /// The timeout of an action that names none: at most [`Config::max_run`].
    pub default_timeout: Duration,
    /// The heartbeat interval the workers' `Welcome` names.
    pub heartbeat: Duration,
    /// How often the caller steps the executor.
    pub poll: Duration,
    /// How long one call to the job service may take before it counts as unanswered. A pass
    /// of the executor is a series of calls, and it carries the heartbeats and the
    /// self-fence, so a service that never answers must not hold it for the client's own
    /// 30 s a call.
    pub call_timeout: Duration,
    /// How long a pass goes on starting waiting tasks: past it the rest wait for the next.
    pub pass_budget: Duration,
    /// T: how long a worker goes on with its jobs after the farm last acknowledged a
    /// heartbeat, or the job service last answered a call about a job it holds, before it
    /// cancels them and goes silent.
    pub fence_after: Duration,
    /// How the secrets reach the task.
    pub delivery: Delivery,
    /// Whether the operator has said the secrets may travel as arguments, where the action
    /// can read them (`/proc/1/cmdline`) and the engine's inspect shows them. False by
    /// default, and [`Config::validate`] refuses a config that leaves it so unless the
    /// delivery is [`Delivery::Files`].
    pub insecure_argv_secrets: bool,
    /// Where the task tool works in the container, if not in the tool's own default: the
    /// kbf image must have a writable directory there.
    pub work_dir: Option<String>,
    /// Whether the task may `PUT` over plain http.
    pub allow_plain_http_put: bool,
    /// Whether the task may read its inputs through a read grant served over plain http, and
    /// the executor may submit a job whose grant url is `http://` (the bearer token then
    /// crosses the job network in clear).
    pub allow_plain_http_grant: bool,
    /// Whether the task is told to read its inputs where `GET /locate` says they are kept
    /// (`--locate`), and from the listener when that read fails.
    pub locate: bool,
    /// The most bytes of a pack the frontend takes.
    pub max_pack_bytes: u64,
    /// The most bytes of one blob in a pack.
    pub max_blob_bytes: u64,
    /// The longest a pack url may stay good: the store's presign ceiling.
    pub presign_ceiling: Duration,
    /// The scratch disk a job needs for what it writes, in bytes, on top of its inputs: the
    /// outputs, the pack it builds and the private home, tmp and cache of the work directory
    /// (`--executor-output-disk-bytes`). A job needs this, the bytes of its input closure and
    /// [`DISK_MARGIN`] ([`Config::disk_bytes`]).
    pub output_disk_bytes: u64,
    /// The network every job is given (`--executor-network`).
    pub network: Network,
    /// Whether the operator has said a job may have an open network
    /// (`--executor-allow-open-network`). [`Config::validate`] refuses
    /// [`Network::Open`] without it, and it without [`Network::Open`].
    pub allow_open_network: bool,
}

/// Why a [`Config`] is refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// A name that cannot be a label value.
    #[error("the frontend id must be 1 to 63 of letters, digits, '-', '_' and '.'")]
    Frontend,
    /// No virtual worker.
    #[error("at least one virtual worker is needed")]
    NoHosts,
    /// No CPU, no slot or more slots than a worker takes.
    #[error("a virtual worker needs at least 1000 millicores and 1 to 256 slots")]
    Shape,
    /// The shape leaves no whole GiB to book after the jobs' hard-cap padding.
    #[error("the virtual worker's memory leaves nothing to book after {0} slots")]
    Memory(u64),
    /// A heartbeat interval the farm refuses.
    #[error("the heartbeat interval must be 1 ms to {MAX_HEARTBEAT_INTERVAL_MS} ms")]
    Heartbeat,
    /// The fence time does not leave the scheduler its margin before it requeues.
    #[error("the fence time plus the margin and one step must be under the lease grace")]
    Fence,
    /// A fence time that is not longer than the heartbeat it counts from.
    #[error("the fence time must exceed the heartbeat interval")]
    FenceShort,
    /// A job whose pack url the store would not sign.
    #[error("the longest job's wall limit exceeds the presign ceiling")]
    Presign,
    /// A default timeout of zero or over the longest run.
    #[error("the default timeout must be above zero and no more than the longest run")]
    Timeout,
    /// A fence time that leaves a `Start` posted at the end of its window a run that outlives
    /// the grace after which the farm runs it again.
    #[error("the start window plus the fence time and the margin must be under the start grace")]
    StartWindow,
    /// A call or pass bound that is zero, or a call that may outlast the fence time.
    #[error(
        "the call timeout must be above zero and under the fence time, and the pass budget above zero"
    )]
    Bounds,
    /// Secrets as arguments, which the operator has not allowed.
    #[error("the secrets travel as arguments: set insecure_argv_secrets to say so")]
    InsecureArgv,
    /// Secrets as files, and the opt-in for secrets as arguments: a contradiction.
    #[error("the secrets travel as files: insecure_argv_secrets must not be set")]
    InsecureWithFiles,
    /// A presign ceiling or a job longer than a url may be good for.
    #[error("the presign ceiling and the longest job's wall limit must be at most 4 hours")]
    PresignLimit,
    /// An open network for the jobs, which the operator has not allowed.
    #[error(
        "--executor-network open lets every job reach any address: give \
         --executor-allow-open-network as well to say so"
    )]
    OpenNetwork,
    /// The opt-in for an open network, with another network: it would open the network on a
    /// later change of one flag.
    #[error("--executor-allow-open-network is only for --executor-network open: drop it")]
    OpenNetworkUnused,
}

impl Config {
    /// A config for `frontend` with `hosts` virtual workers of `cpu_millis` and
    /// `memory_bytes`, with the defaults of every other setting.
    #[must_use]
    pub fn new(frontend: &str, hosts: usize, cpu_millis: u64, memory_bytes: u64) -> Self {
        Self {
            frontend: frontend.to_owned(),
            instance: instance_id(),
            hosts,
            cpu_millis,
            memory_bytes,
            slots: 8,
            arch: "x86_64".to_owned(),
            max_run: Duration::from_secs(3000),
            run_margin: Duration::from_secs(300),
            default_timeout: Duration::from_secs(3000),
            heartbeat: Duration::from_secs(5),
            poll: Duration::from_millis(500),
            call_timeout: Duration::from_secs(5),
            pass_budget: Duration::from_secs(10),
            fence_after: SELF_FENCE,
            delivery: Delivery::InsecureArgv,
            insecure_argv_secrets: false,
            work_dir: None,
            allow_plain_http_put: false,
            allow_plain_http_grant: false,
            locate: false,
            max_pack_bytes: 128 << 20,
            max_blob_bytes: 64 << 20,
            presign_ceiling: PRESIGN_LIMIT,
            output_disk_bytes: DEFAULT_OUTPUT_DISK,
            network: Network::AllowList,
            allow_open_network: false,
        }
    }

    /// How long a process that finds live jobs of another process under its own frontend id
    /// watches for a new one before it cancels them: a live executor's workers submit what the
    /// farm places on them within a pass, and a pass is held at most the fence time
    /// ([`Config::fence_after`]: a worker that cannot reach the job service for longer has
    /// fenced itself), so the watch is that and a step. It is also how often the executor
    /// looks for another frontend's jobs ([`Executor::rival_check`](super::Executor::rival_check)).
    ///
    /// An idle process makes no job, so the watch cannot tell it from a gone one: its jobs are
    /// cancelled, and the rival check stops this process once the other submits again.
    #[must_use]
    pub fn adopt_watch(&self) -> Duration {
        self.fence_after + self.poll
    }

    /// Checks what the executor's safety rests on.
    ///
    /// The fence time T plus the scheduler's margin plus one step must stay under the
    /// scheduler's re-dispatch grace G ([`LEASE_GRACE`]): a worker the farm last heard from
    /// is silent after T, and its jobs are cancelled by then, so the farm that requeues
    /// them after G does not run the action beside the first copy. The cancel goes to the job
    /// service; if that is the one that cannot be reached, the job goes on to its wall limit,
    /// which wastes the work and is harmless: its pack key is its own.
    ///
    /// A `Start` submitted at the end of its window ([`START_VALIDITY`]) is then a job whose
    /// cancel is sent T later, and the farm runs the action again [`START_GRACE`] after the
    /// `Start`: the window, T and the margin stay under it.
    ///
    /// # Errors
    /// The first setting that breaks a rule.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let name_ok = |c: char| c.is_ascii_alphanumeric() || "-_.".contains(c);
        let frontend =
            (1..=63).contains(&self.frontend.len()) && self.frontend.chars().all(name_ok);
        let heartbeat = u64::try_from(self.heartbeat.as_millis()).unwrap_or(u64::MAX);
        let longest = self.max_run + self.run_margin;
        let checks = [
            (frontend, ConfigError::Frontend),
            (self.hosts >= 1, ConfigError::NoHosts),
            (
                self.cpu_millis >= 1_000 && (1..=256).contains(&self.slots),
                ConfigError::Shape,
            ),
            (
                self.booked_memory_gib() >= 1,
                ConfigError::Memory(self.slots),
            ),
            (
                (1..=MAX_HEARTBEAT_INTERVAL_MS).contains(&heartbeat),
                ConfigError::Heartbeat,
            ),
            (self.fence_after > self.heartbeat, ConfigError::FenceShort),
            (
                self.fence_after + LEADER_LEASE_MARGIN + self.poll < LEASE_GRACE,
                ConfigError::Fence,
            ),
            (
                !self.default_timeout.is_zero() && self.default_timeout <= self.max_run,
                ConfigError::Timeout,
            ),
            (self.presign_ceiling >= longest, ConfigError::Presign),
            (
                self.presign_ceiling <= PRESIGN_LIMIT,
                ConfigError::PresignLimit,
            ),
            (
                START_VALIDITY + self.fence_after + LEADER_LEASE_MARGIN < START_GRACE,
                ConfigError::StartWindow,
            ),
            (
                !self.call_timeout.is_zero()
                    && self.call_timeout < self.fence_after
                    && !self.pass_budget.is_zero(),
                ConfigError::Bounds,
            ),
            (
                self.delivery == Delivery::Files || self.insecure_argv_secrets,
                ConfigError::InsecureArgv,
            ),
            (
                self.delivery != Delivery::Files || !self.insecure_argv_secrets,
                ConfigError::InsecureWithFiles,
            ),
            (
                self.network != Network::Open || self.allow_open_network,
                ConfigError::OpenNetwork,
            ),
            (
                self.network == Network::Open || !self.allow_open_network,
                ConfigError::OpenNetworkUnused,
            ),
        ];
        checks
            .into_iter()
            .find_map(|(ok, error)| (!ok).then_some(error))
            .map_or(Ok(()), Err)
    }

    /// What a job asks of its host beside resources: the system and the architecture the
    /// worker's node report names, in a container. The task tool and its static binary run
    /// nowhere else.
    #[must_use]
    pub fn constraints(&self) -> Constraints {
        Constraints {
            os: "linux".to_owned(),
            arch: self.arch.clone(),
            exec: ExecMode::Container.into(),
        }
    }

    /// How many whole GiB of memory the farm may book on a virtual worker.
    ///
    /// A job needs 1.5 times its booking plus [`HARD_CAP_PAD`]. Booking `m` bytes over at most
    /// `slots` jobs needs `1.5 m + slots * pad`, which stays within the shape, so what the farm
    /// has in flight on one worker fits a host of that shape.
    #[must_use]
    pub fn booked_memory_gib(&self) -> u64 {
        let usable = self
            .memory_bytes
            .saturating_sub(self.slots.saturating_mul(HARD_CAP_PAD));
        usable / 3 * 2 / GIB
    }

    /// The hard memory cap of a job the farm booked `booking` bytes for: what the job needs.
    #[must_use]
    pub fn hard_memory(booking: u64) -> u64 {
        booking.saturating_mul(3) / 2 + HARD_CAP_PAD
    }

    /// The scratch disk a job needs: its `input_bytes` (`None` when the size of its input
    /// closure is not known), [`Config::output_disk_bytes`] and [`DISK_MARGIN`].
    #[must_use]
    pub fn disk_bytes(&self, input_bytes: Option<u64>) -> u64 {
        input_bytes
            .unwrap_or(0)
            .saturating_add(self.output_disk_bytes)
            .saturating_add(DISK_MARGIN)
    }

    /// The wall limit of a job of an action of `timeout`: the action's own, and nothing cuts
    /// it shorter, so a job stopped at it is the action's timeout.
    #[must_use]
    pub fn wall(&self, timeout: Duration) -> Duration {
        timeout + self.run_margin
    }

    /// The `Hello` of a virtual worker: what a real node of the shape would report, and no
    /// more. The only driver is `container`, so no whole-machine or VM lease is placed on it.
    pub(super) fn hello(&self, node: &str, instance: &str, predecessor: &str) -> Hello {
        let entry = |key: &str, value: String| Capability {
            key: key.to_owned(),
            value,
        };
        Hello {
            protocol_version: PROTOCOL_VERSION,
            node_id: node.to_owned(),
            daemon_version: "kbf-server executor".to_owned(),
            capabilities: vec![
                entry("action_slots", self.slots.to_string()),
                entry("arch", self.arch.clone()),
                entry("cpus", (self.cpu_millis / 1_000).to_string()),
                entry("drivers", "container".to_owned()),
                entry("mem_gib", self.booked_memory_gib().to_string()),
                entry("os", "linux".to_owned()),
            ],
            instance_id: instance.to_owned(),
            predecessor_instance_id: predecessor.to_owned(),
            ..Hello::default()
        }
    }
}

/// An id of this process, in hex: 128 random bits from the operating system's source, then
/// the process id and the time. A source that fails (which nothing here can repair) leaves
/// the bits zero, and the process id and the time still tell two processes apart.
fn instance_id() -> String {
    let mut bytes = [0_u8; 16];
    let _ = SystemRandom::new().fill(&mut bytes);
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "{:032x}{:08x}{:x}",
        u128::from_be_bytes(bytes),
        std::process::id(),
        since.as_nanos()
    )
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
