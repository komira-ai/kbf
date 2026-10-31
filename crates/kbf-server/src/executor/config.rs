//! What the executor is configured with, and what follows from it: the node report of a
//! host, the memory cap of a run, the wall time of a run, and the timers, which are
//! checked against the scheduler's own.

use std::time::Duration;

use kbf_proto::worker::{Capability, Hello};
use kbf_sched::fence::{LEADER_LEASE_MARGIN, LEASE_GRACE, SELF_FENCE, START_GRACE, START_VALIDITY};

use crate::config::MAX_HEARTBEAT_INTERVAL_MS;
use crate::executor::task::Delivery;
use crate::worker::PROTOCOL_VERSION;

const GIB: u64 = 1 << 30;

/// The longest a presigned url may be asked to stay good. The store refuses more, and the
/// executor does not ask: [`Config::presign_ceiling`] stays under it, and the longest run's
/// wall time under that.
pub const PRESIGN_LIMIT: Duration = Duration::from_secs(4 * 3600);

/// What the container driver adds to 1.5 times a booking for the hard memory cap of a run
/// (`kbf-driver-container`, `cgroup.rs`). The lease server caps a run at the number it is
/// sent, so the executor sends the same one and an out-of-memory run is the same event
/// on either executor.
const HARD_CAP_PAD: u64 = 512 << 20;

/// What a run books for its outputs when the operator says nothing: 8 GiB. A host reports its
/// scratch as one share for all its runs, so this times the host's slots (and the margin, and
/// the inputs) is what a full host asks of it.
pub const DEFAULT_OUTPUT_DISK: u64 = 8 * GIB;

/// What a run books beyond its inputs and its output allowance: 1 GiB for what neither counts
/// (the task tool's own files, the manifest and logs, the filesystem's rounding).
pub const DISK_MARGIN: u64 = GIB;

/// The executor's settings. Every duration is an upper or lower bound the executor holds
/// itself to; [`Config::validate`] refuses a set that breaks the ones the scheduler's
/// safety rests on.
#[derive(Clone, Debug)]
pub struct Config {
    /// This frontend's id: the value of the `kbf-frontend` label on its leases, the prefix
    /// of its workers' instances.
    pub frontend: String,
    /// Which process of this frontend id this is: the value of the `kbf-instance` label on its
    /// leases, made up at start. A lease of another process under the same frontend id is never
    /// taken for this one's while that process renews it.
    pub instance: String,
    /// How many hosts to hold, a lease each.
    pub hosts: usize,
    /// The class every lease asks for.
    pub class: String,
    /// What a host's lease holds in CPU, in thousandths of a core: the pool's `[shape]`.
    pub cpu_millis: u64,
    /// What a host's lease holds in memory, in bytes: the pool's `[shape]`.
    pub memory_bytes: u64,
    /// The most runs a host takes at once (`action_slots` of its node report).
    pub slots: u64,
    /// The CPU architecture of the hosts.
    pub arch: String,
    /// The longest a worker goes between two renewals of its lease, whatever the ttl: a
    /// process that starts under the same frontend id tells a live executor from a dead one's
    /// leftovers by a renewal ([`Config::adopt_watch`]).
    pub renew_every: Duration,
    /// How long a lease lasts without a renewal, asked at each renewal. A run's wall time is
    /// cut to what the lease has left when it is posted, so this must cover the longest run
    /// ([`Config::max_run`] and [`Config::run_margin`]) and two heartbeats more (the server cuts
    /// a run's wall time again from the time left when the run is delivered to the host, which
    /// can be a heartbeat late); the lease server grants at most its
    /// `--max-ttl`, and a worker that is granted less treats a run the lease cuts short as
    /// lost, not as the action's timeout.
    pub lease_ttl: Duration,
    /// How long a lease may last in all: its `max_wall_seconds`.
    pub max_wall: Duration,
    /// How long before the end of its `max_wall` a host stops taking work and is released.
    pub drain_before_end: Duration,
    /// The longest action timeout the executor runs.
    pub max_run: Duration,
    /// What a run's wall time adds to the action's timeout: fetch, pack and upload. The task
    /// tool's own deadline bounds only the action's command, so a fetch of a large input tree
    /// or an upload that outlasts this margin ends the run at the wall time, and is answered
    /// to the caller as a timeout and not rerun: raise it for slower stores.
    pub run_margin: Duration,
    /// The timeout of an action that names none: at most [`Config::max_run`].
    pub default_timeout: Duration,
    /// The heartbeat interval the workers' `Welcome` names.
    pub heartbeat: Duration,
    /// How often the caller steps the executor.
    pub poll: Duration,
    /// How long before a vacant host is asked for again.
    pub acquire_retry: Duration,
    /// How long a host whose scratch quota is out of order (a run refused `quota-unavailable`)
    /// is kept out of use: its lease is held idle, so the lease server hands out another
    /// host, and released when this has passed (at most a third of [`Config::lease_ttl`]: see
    /// [`Config::held_out_for`]). The host recovers by itself and re-checks
    /// about once a minute, so this is a little over two of its checks.
    pub quota_hold: Duration,
    /// How often the lease server's host list is read, to see a host lost or tainted.
    pub host_check: Duration,
    /// How often the leases under this frontend's label are compared with the slots', to
    /// release one no slot holds: one booked by a request whose answer was lost, or one a
    /// release did not reach. The same check runs at once after either.
    pub reconcile_every: Duration,
    /// How long one call to the lease server may take before it counts as unanswered. A pass
    /// of the executor is a series of calls, and it carries the heartbeats and the
    /// self-fence, so a server that never answers must not hold it for the client's own
    /// 30 s a call.
    pub call_timeout: Duration,
    /// How long a pass goes on starting waiting tasks: past it the rest wait for the next.
    pub pass_budget: Duration,
    /// T: how long a worker goes on with its runs after the farm last acknowledged a
    /// heartbeat, or the lease server last answered, before it stops them and goes silent.
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
    /// the executor may post a run whose grant url is `http://` (the bearer token then crosses
    /// the job network in clear).
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
    /// The scratch disk a run books for what it writes, in bytes, on top of its inputs: the
    /// outputs, the pack it builds and the private home, tmp and cache of the work directory
    /// (`--executor-output-disk-bytes`). A run books this, the bytes of its input closure and
    /// [`DISK_MARGIN`] ([`Config::disk_bytes`]).
    pub output_disk_bytes: u64,
}

/// Why a [`Config`] is refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// A name that cannot be a label value.
    #[error("the frontend id must be 1 to 63 of letters, digits, '-', '_' and '.'")]
    Frontend,
    /// No host to hold.
    #[error("at least one host must be held")]
    NoHosts,
    /// No CPU, no slot or more slots than a host takes.
    #[error("a host needs at least 1000 millicores and 1 to 256 slots")]
    Shape,
    /// The shape leaves no whole GiB to book after the runs' hard-cap padding.
    #[error("the host's memory leaves nothing to book after {0} slots")]
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
    /// A drain that starts too late for the longest run to end before the lease does.
    #[error("the drain must start at least a run's wall time before the lease ends")]
    Drain,
    /// A run whose pack url the store would not sign.
    #[error("the longest run's wall time exceeds the presign ceiling")]
    Presign,
    /// A lease ttl that cannot be renewed in time, or that does not cover the longest run.
    #[error(
        "the lease ttl must be at least 30 s, three times the step, and cover the longest run and two heartbeats"
    )]
    Ttl,
    /// A hold on a host with a sick quota of zero.
    #[error("the quota hold must be above zero")]
    QuotaHold,
    /// A renewal interval of zero.
    #[error("the renewal interval must be above zero")]
    Renew,
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
    /// A presign ceiling or a run longer than a url may be good for.
    #[error("the presign ceiling and the longest run's wall time must be at most 4 hours")]
    PresignLimit,
}

impl Config {
    /// A config for `frontend` holding `hosts` hosts of `cpu_millis` and `memory_bytes`
    /// under `class`, with the defaults of every other setting.
    #[must_use]
    pub fn new(
        frontend: &str,
        hosts: usize,
        class: &str,
        cpu_millis: u64,
        memory_bytes: u64,
    ) -> Self {
        Self {
            frontend: frontend.to_owned(),
            instance: super::Nonce::random().to_string(),
            hosts,
            class: class.to_owned(),
            cpu_millis,
            memory_bytes,
            slots: 8,
            arch: "x86_64".to_owned(),
            renew_every: Duration::from_secs(30),
            lease_ttl: Duration::from_secs(3600),
            max_wall: Duration::from_secs(6 * 3600),
            drain_before_end: Duration::from_secs(90 * 60),
            max_run: Duration::from_secs(3000),
            run_margin: Duration::from_secs(300),
            default_timeout: Duration::from_secs(3000),
            heartbeat: Duration::from_secs(5),
            poll: Duration::from_millis(500),
            acquire_retry: Duration::from_secs(5),
            quota_hold: Duration::from_secs(150),
            host_check: Duration::from_secs(5),
            reconcile_every: Duration::from_secs(60),
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
        }
    }

    /// How long a lease is kept idle for a host with a sick quota: [`Config::quota_hold`], cut
    /// to a third of the lease ttl. The idle lease is not renewed, and a renewed lease has at
    /// least a third of its ttl left.
    #[must_use]
    pub fn held_out_for(&self) -> Duration {
        self.quota_hold.min(self.lease_ttl / 3)
    }

    /// How long a process that finds another's leases under its own frontend id watches them
    /// before it takes them: the longest a live executor goes between two renewals of a lease
    /// ([`Config::renew_every`]), plus the longest a pass can be held ([`Config::fence_after`]:
    /// a worker that cannot reach the lease server for longer has fenced itself and is no
    /// longer a holder), plus a step.
    #[must_use]
    pub fn adopt_watch(&self) -> Duration {
        self.renew_every + self.fence_after + self.poll
    }

    /// Checks what the executor's safety rests on.
    ///
    /// The fence time T plus the scheduler's margin plus one step must stay under the
    /// scheduler's re-dispatch grace G ([`LEASE_GRACE`]): a worker the farm last heard from
    /// is silent after T, and its runs are asked to stop by then, so the farm that requeues
    /// them after G does not run the action beside the first copy. The ask goes to the lease
    /// server; if that is the one that cannot be reached, the run goes on to its wall time,
    /// which wastes the work and is harmless: its pack key is its own.
    ///
    /// A `Start` posted at the end of its window ([`START_VALIDITY`]) is then a run whose
    /// stop is asked T later, and the farm runs the action again [`START_GRACE`] after the
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
                [
                    self.lease_ttl >= Duration::from_secs(30),
                    self.lease_ttl >= self.poll * 3,
                    self.lease_ttl >= longest + self.heartbeat * 2,
                ]
                .into_iter()
                .all(|ok| ok),
                ConfigError::Ttl,
            ),
            (!self.renew_every.is_zero(), ConfigError::Renew),
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
                self.max_wall > self.drain_before_end && self.drain_before_end > longest,
                ConfigError::Drain,
            ),
            (!self.quota_hold.is_zero(), ConfigError::QuotaHold),
        ];
        checks
            .into_iter()
            .find_map(|(ok, error)| (!ok).then_some(error))
            .map_or(Ok(()), Err)
    }

    /// What a lease must ask of its host: the system and the architecture, the same as the
    /// worker's node report says, and under [`Delivery::Files`] `run-files`, so that a host
    /// that cannot mount files is never leased.
    #[must_use]
    pub fn host_capabilities(&self) -> std::collections::BTreeMap<String, String> {
        let files = (self.delivery == Delivery::Files).then_some(("run-files", "true"));
        [("os", "linux"), ("arch", self.arch.as_str())]
            .into_iter()
            .chain(files)
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect()
    }

    /// How many whole GiB of memory the farm may book on a host.
    ///
    /// A run is capped at 1.5 times its booking plus [`HARD_CAP_PAD`], and the lease server
    /// admits runs while the caps add up to the shape. Booking `m` bytes over at most
    /// `slots` runs caps them at `1.5 m + slots * pad`, which must stay within the shape.
    #[must_use]
    pub fn booked_memory_gib(&self) -> u64 {
        let usable = self
            .memory_bytes
            .saturating_sub(self.slots.saturating_mul(HARD_CAP_PAD));
        usable / 3 * 2 / GIB
    }

    /// The hard memory cap of a run the farm booked `booking` bytes for.
    #[must_use]
    pub fn hard_memory(booking: u64) -> u64 {
        booking.saturating_mul(3) / 2 + HARD_CAP_PAD
    }

    /// The scratch disk a run books: its `input_bytes` (`None` when the size of its input
    /// closure is not known), [`Config::output_disk_bytes`] and [`DISK_MARGIN`].
    #[must_use]
    pub fn disk_bytes(&self, input_bytes: Option<u64>) -> u64 {
        input_bytes
            .unwrap_or(0)
            .saturating_add(self.output_disk_bytes)
            .saturating_add(DISK_MARGIN)
    }

    /// The wall time of a run of an action of `timeout`. Never the time a lease has left:
    /// the run a stop cannot reach ends by itself by then.
    #[must_use]
    pub fn wall(&self, timeout: Duration) -> Duration {
        timeout + self.run_margin
    }

    /// The `Hello` of a worker on a host: what a real node would report, and no more. The
    /// only driver is `container`, so no whole-machine or VM lease is placed on it.
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

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
