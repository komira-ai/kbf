//! What the executor is configured with, and what follows from it: the node report of a
//! host, the memory cap of a run, the wall time of a run, and the timers, which are
//! checked against the scheduler's own.

use std::time::Duration;

use kbf_lease_client::Mount;
use kbf_proto::worker::{Capability, Hello};
use kbf_sched::fence::{LEADER_LEASE_MARGIN, LEASE_GRACE, SELF_FENCE};

use crate::config::MAX_HEARTBEAT_INTERVAL_MS;
use crate::executor::task::Delivery;
use crate::worker::PROTOCOL_VERSION;

const GIB: u64 = 1 << 30;

/// What the container driver adds to 1.5 times a booking for the hard memory cap of a run
/// (`kbf-driver-container`, `cgroup.rs`). The lease server caps a run at the number it is
/// sent, so the executor sends the same one and an out-of-memory run is the same event
/// on either executor.
const HARD_CAP_PAD: u64 = 512 << 20;

/// The executor's settings. Every duration is an upper or lower bound the executor holds
/// itself to; [`Config::validate`] refuses a set that breaks the ones the scheduler's
/// safety rests on.
#[derive(Clone, Debug)]
pub struct Config {
    /// This frontend's id: the value of the `kbf-frontend` label on its leases, the prefix
    /// of its workers' instances and the build of its pack keys.
    pub frontend: String,
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
    /// How long a lease lasts without a renewal. At most the server's `--default-ttl`.
    pub lease_ttl: Duration,
    /// How long a lease may last in all: its `max_wall_seconds`.
    pub max_wall: Duration,
    /// How long before the end of its `max_wall` a host stops taking work and is released.
    pub drain_before_end: Duration,
    /// The longest action timeout the executor runs.
    pub max_run: Duration,
    /// What a run's wall time adds to the action's timeout: fetch, pack and upload.
    pub run_margin: Duration,
    /// The wall time of an action that names no timeout.
    pub default_timeout: Duration,
    /// The heartbeat interval the workers' `Welcome` names.
    pub heartbeat: Duration,
    /// How often the caller steps the executor.
    pub poll: Duration,
    /// How long before a vacant host is asked for again.
    pub acquire_retry: Duration,
    /// How often the lease server's host list is read, to see a host lost or tainted.
    pub host_check: Duration,
    /// T: how long a worker goes on with its runs after the farm last acknowledged a
    /// heartbeat, or the lease server last answered, before it stops them and goes silent.
    pub fence_after: Duration,
    /// How the secrets reach the task.
    pub delivery: Delivery,
    /// The path of the task tool in the container.
    pub tool: String,
    /// The mount that puts the tool there.
    pub tool_mount: Mount,
    /// The farm's root certificate in the container, for the task's `https` reads.
    pub root_der: Option<String>,
    /// Whether the task may `PUT` over plain http.
    pub allow_plain_http_put: bool,
    /// The most bytes of a pack the frontend takes.
    pub max_pack_bytes: u64,
    /// The most bytes of one blob in a pack.
    pub max_blob_bytes: u64,
    /// The longest a pack url may stay good: the store's presign ceiling.
    pub presign_ceiling: Duration,
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
    /// A lease ttl that cannot be renewed in time.
    #[error("the lease ttl must be at least 30 s and three times the step")]
    Ttl,
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
            hosts,
            class: class.to_owned(),
            cpu_millis,
            memory_bytes,
            slots: 8,
            arch: "x86_64".to_owned(),
            lease_ttl: Duration::from_secs(600),
            max_wall: Duration::from_secs(6 * 3600),
            drain_before_end: Duration::from_secs(90 * 60),
            max_run: Duration::from_secs(3600),
            run_margin: Duration::from_secs(300),
            default_timeout: Duration::from_secs(3600),
            heartbeat: Duration::from_secs(5),
            poll: Duration::from_millis(500),
            acquire_retry: Duration::from_secs(5),
            host_check: Duration::from_secs(5),
            fence_after: SELF_FENCE,
            delivery: Delivery::InsecureArgv,
            tool: "/mnt/kbf-tools/kbf-task".to_owned(),
            tool_mount: Mount {
                root_name: "kbf-tools".to_owned(),
                rel: "kbf-task".to_owned(),
                ro: true,
            },
            root_der: None,
            allow_plain_http_put: false,
            max_pack_bytes: 128 << 20,
            max_blob_bytes: 64 << 20,
            presign_ceiling: Duration::from_secs(4 * 3600),
        }
    }

    /// Checks what the executor's safety rests on.
    ///
    /// The fence time T plus the scheduler's margin plus one step must stay under the
    /// scheduler's re-dispatch grace G ([`LEASE_GRACE`]): a worker the farm last heard from
    /// is silent after T, and its runs are stopped by then, so the farm that requeues them
    /// after G never runs the action beside the first copy.
    ///
    /// # Errors
    /// The first setting that breaks a rule.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let name_ok = |c: char| c.is_ascii_alphanumeric() || "-_.".contains(c);
        let frontend = (1..=63).contains(&self.frontend.len()) && self.frontend.chars().all(name_ok);
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
                self.lease_ttl >= Duration::from_secs(30) && self.lease_ttl >= self.poll * 3,
                ConfigError::Ttl,
            ),
            (self.presign_ceiling >= longest, ConfigError::Presign),
            (
                self.max_wall > self.drain_before_end && self.drain_before_end > longest,
                ConfigError::Drain,
            ),
        ];
        checks
            .into_iter()
            .find_map(|(ok, error)| (!ok).then_some(error))
            .map_or(Ok(()), Err)
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
