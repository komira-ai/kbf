//! What the executor is configured with, and what follows from it: the node report of a
//! virtual worker, the needs and the constraints of a job, the wall limit of a job, and the
//! timers, which are checked against the scheduler's own.

use std::time::Duration;

use js_proto::v1::{Constraints, ExecMode, Isolation};
use kbf_proto::worker::{Capability, Hello};
use kbf_sched::fence::{LEADER_LEASE_MARGIN, LEASE_GRACE, SELF_FENCE, START_GRACE, START_VALIDITY};
use ring::rand::{SecureRandom, SystemRandom};

use crate::config::MAX_HEARTBEAT_INTERVAL_MS;
use crate::executor::mac::{MAC_ARCH, MAC_DRIVER, MAC_OS, MacPool, MacPoolError, WorkerOs};
use crate::executor::out::PutDirRoots;
use crate::worker::PROTOCOL_VERSION;

const GIB: u64 = 1 << 30;

/// The default of [`Config::failed_stderr_log_bytes`]: enough for the line a tool dies with.
pub const FAILED_STDERR_LOG_BYTES: usize = 512;

/// The drivers a virtual worker's `Hello` lists, in the spelling the scheduler reads a lease
/// kind's drivers by (`LeaseKind::drivers`): `container` serves an `action` lease, which
/// becomes a job that needs a share of a host, and `native-whole-machine` serves a
/// `whole_machine` lease, which becomes a job that needs the whole host. No VM driver: no VM
/// lease is placed on it.
pub(super) const DRIVERS: [&str; 2] = ["container", "native-whole-machine"];

/// The longest the keys of a put directory may be asked to stay good. The store refuses more,
/// and the executor does not ask: [`Config::key_ceiling`] stays under it, and the longest job's
/// wall limit under that.
pub const KEY_LIMIT: Duration = Duration::from_secs(4 * 3600);

/// The default heartbeat interval. A `Start` names the newest heartbeat the farm took and is
/// valid for [`START_VALIDITY`] from it, so the age of that heartbeat when the `Start` arrives
/// is time out of the window the preparation and the submit have: at 5 s it was up to 5 s of
/// 14, and many grants were dropped as late; at 1 s it is up to 1 s.
const HEARTBEAT: Duration = Duration::from_secs(1);

/// What the removed daemon's container driver added to 1.5 times a booking for the hard memory
/// cap of a run. A Mac job needs the memory it is sent and its host
/// caps it there, so the executor sends the same number and an out-of-memory job is the same
/// event as an out-of-memory run on a worker. A Linux job names no memory: the job service
/// sizes it from its class's history.
const HARD_CAP_PAD: u64 = 512 << 20;

/// Who owns the jobs of the executor's workers when several frontends share one state store
/// (`--executor-ownership`, `docs/design/n-frontends.md`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Ownership {
    /// One frontend per frontend id: it looks for another frontend's jobs and stops if it
    /// finds one, cancels at start the jobs an earlier process of its id left, and cancels its
    /// jobs when it ends or fences. The default.
    #[default]
    Single,
    /// Any number of frontends under one frontend id (the group), over one state store. Each
    /// process names its workers by its own instance, writes the `exec/` rows of its workers
    /// and jobs, renews its liveness row, and never cancels a job but on the farm's `Cancel` or
    /// a withdrawal (the farm ending a worker's stream withdraws all its leases): not at start,
    /// not when it fences, not when it ends.
    Shared,
}

/// What a put-directory job's root is read from (`--executor-input-tree`,
/// docs/design/actions-as-jobs.md, 4.7).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum InputTree {
    /// The action tree written as tierstore tree manifests, the job's layer the host directory
    /// `<mp>/trees/<digest>` that tierstore's file system serves. The default.
    #[default]
    Manifests,
    /// The action tree written as one tierstore tree list, the job's layer tierstore's
    /// mount provider with the list's key: the host's agent asks the provider for the tree
    /// as soon as the job is placed, and its files are fetched while the sandbox is made. A Mac
    /// job's tree is always [`InputTree::Manifests`]: its runtime takes no provider yet.
    Provider,
}

/// The executor's settings. Every duration is an upper or lower bound the executor holds
/// itself to; [`Config::validate`] refuses a set that breaks the ones the scheduler's
/// safety rests on.
#[derive(Clone, Debug)]
pub struct Config {
    /// This frontend's id: the value of the `kbf-frontend` label on its jobs, hashed into
    /// their ids, and the name of its virtual workers ([`Config::worker_name`]). With
    /// [`Ownership::Shared`] it names the group, and every frontend of the group gives the same.
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
    /// The most bytes of a failed job's standard error the frontend writes to its log, at WARN,
    /// the end of the stream. `0` writes none; the whole attached output stays with the result
    /// either way.
    pub failed_stderr_log_bytes: usize,
    /// The heartbeat interval the workers' `Welcome` names: [`HEARTBEAT`] by default.
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
    /// How long the inputs of one run (its action, its input tree published into the store,
    /// its put directory) may take to prepare. A preparation is not stopped by the `Start`'s
    /// window, which only the submit must meet: it goes on until this ends, so a tree published by a
    /// run whose window passed is there for the run the farm starts next.
    pub prepare_limit: Duration,
    /// How many of one worker's preparations run at once. A worker the farm gave a full set of
    /// `Start`s at one instant prepared them all together, each slowed by the rest until none
    /// was ready inside the window; the `Start`s past this wait their turn, and are dropped
    /// at their window, unprepared, only if their turn did not come.
    pub prepare_concurrency: usize,
    /// T: how long a worker goes on with its jobs after the farm last acknowledged a
    /// heartbeat, or the job service last answered a call about a job it holds (or, with
    /// [`Ownership::Shared`], the state store last took a renewal of the liveness row), before
    /// it cancels them (with [`Ownership::Shared`], drops them) and goes silent.
    pub fence_after: Duration,
    /// The most bytes of a pack the frontend takes.
    pub max_pack_bytes: u64,
    /// The most bytes of one blob in a pack.
    pub max_blob_bytes: u64,
    /// The most records of a one-pass object (`--task-max-object-entries`).
    pub max_object_entries: usize,
    /// The most bytes of a one-pass object's manifest (`--task-max-object-manifest-bytes`).
    pub max_object_manifest_bytes: u64,
    /// The bytes of a one-pass object's tail a record may account for
    /// (`--task-object-bytes-per-entry`).
    pub object_bytes_per_entry: u64,
    /// The longest a put directory's keys may be good for.
    pub key_ceiling: Duration,
    /// The host directories of the put directory of every run (`--task-out-root`). `None`, the
    /// default, refuses every Linux and Mac run as the caller's: a run has nowhere to commit.
    /// A job has no network.
    pub task_out: Option<PutDirRoots>,
    /// Who owns the jobs ([`Ownership`]; `--executor-ownership`).
    pub ownership: Ownership,
    /// What a Linux put-directory job's root is read from ([`InputTree`];
    /// `--executor-input-tree`).
    pub input_tree: InputTree,
    /// How long the liveness row of this process lives after a renewal, on the store's clock
    /// (`--executor-live-secs`): with [`Ownership::Shared`], at least
    /// [`Config::fence_after`] and [`LEADER_LEASE_MARGIN`], so the process has stopped acting
    /// before its row can expire.
    pub live: Duration,
    /// The Macs (`--executor-mac-*`): `None`, the default, registers no Mac worker, and the farm
    /// places no Mac action on this executor ([`crate::executor::mac`]).
    pub mac: Option<MacPool>,
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
    /// A job whose put directory's keys would not stay good for its wall limit.
    #[error("the longest job's wall limit exceeds the key ceiling")]
    KeyCeiling,
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
    /// A key ceiling or a job longer than a put directory's keys may be good for.
    #[error("the key ceiling and the longest job's wall limit must be at most 4 hours")]
    KeyLimit,
    /// A liveness row that may expire while its process still acts.
    #[error(
        "--executor-live-secs must be at least the fence time plus {} s with \
         --executor-ownership shared",
        LEADER_LEASE_MARGIN.as_secs()
    )]
    Live,
    /// Shared ownership with no state store to keep the rows in.
    #[error("--executor-ownership shared needs --state-store: the rows of the jobs are kept there")]
    SharedStore,
    /// The pool of Macs is not one.
    #[error("{0}")]
    Mac(#[from] MacPoolError),
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
            failed_stderr_log_bytes: FAILED_STDERR_LOG_BYTES,
            heartbeat: HEARTBEAT,
            poll: Duration::from_millis(1000),
            call_timeout: Duration::from_secs(5),
            pass_budget: Duration::from_secs(10),
            prepare_limit: Duration::from_secs(300),
            prepare_concurrency: 4,
            fence_after: SELF_FENCE,
            max_pack_bytes: 128 << 20,
            max_blob_bytes: 64 << 20,
            max_object_entries: 1 << 20,
            max_object_manifest_bytes: 4 << 20,
            object_bytes_per_entry: 1024,
            key_ceiling: KEY_LIMIT,
            task_out: None,
            ownership: Ownership::Single,
            input_tree: InputTree::Manifests,
            live: SELF_FENCE + LEADER_LEASE_MARGIN,
            mac: None,
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
            (self.key_ceiling >= longest, ConfigError::KeyCeiling),
            (self.key_ceiling <= KEY_LIMIT, ConfigError::KeyLimit),
            (
                START_VALIDITY + self.fence_after + LEADER_LEASE_MARGIN < START_GRACE,
                ConfigError::StartWindow,
            ),
            (
                !self.call_timeout.is_zero()
                    && self.call_timeout < self.fence_after
                    && !self.pass_budget.is_zero()
                    && !self.prepare_limit.is_zero()
                    && self.prepare_concurrency > 0,
                ConfigError::Bounds,
            ),
            (
                !self.shared() || self.live >= self.fence_after + LEADER_LEASE_MARGIN,
                ConfigError::Live,
            ),
        ];
        let first = checks
            .into_iter()
            .find_map(|(ok, error)| (!ok).then_some(error));
        match (first, &self.mac) {
            (Some(error), _) => Err(error),
            (None, Some(mac)) => Ok(mac.validate()?),
            (None, None) => Ok(()),
        }
    }

    /// Whether the jobs are owned by the group ([`Ownership::Shared`]).
    #[must_use]
    pub fn shared(&self) -> bool {
        self.ownership == Ownership::Shared
    }

    /// The id of the `n`th Linux virtual worker: `js:<frontend>-<n>` for one frontend per id,
    /// and `js:<frontend>.<the first 8 characters of the instance>-<n>` when the group shares
    /// the id, so two live processes never register one worker.
    #[must_use]
    pub fn worker_name(&self, n: usize) -> String {
        self.worker_of(WorkerOs::Linux, n)
    }

    /// The id of the `n`th virtual worker for `os`: [`Config::worker_name`] for Linux, and the
    /// same under the prefix `js-mac:` for the Macs, so no frontend id makes one the other's.
    pub(super) fn worker_of(&self, os: WorkerOs, n: usize) -> String {
        let kind = match os {
            WorkerOs::Linux => "js",
            WorkerOs::Mac => "js-mac",
        };
        match self.ownership {
            Ownership::Single => format!("{kind}:{}-{n}", self.frontend),
            Ownership::Shared => {
                let short: String = self.instance.chars().take(8).collect();
                format!("{kind}:{}.{short}-{n}", self.frontend)
            }
        }
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
            // No ISA level, no accelerator, and confinement left unspecified (contained):
            // the request's bytes stay those an older job service also reads.
            isa_level: String::new(),
            accelerator: None,
            isolation: Isolation::Unspecified.into(),
        }
    }

    /// How many whole GiB of memory the farm may book on a virtual worker.
    ///
    /// A Mac job needs 1.5 times its booking plus [`HARD_CAP_PAD`]. Booking `m` bytes over at
    /// most `slots` jobs needs `1.5 m + slots * pad`, which stays within the shape, so what the
    /// farm has in flight on one worker fits a host of that shape. A Linux job's own size is the
    /// job service's (its class's history); the farm's booking only paces how many it sends.
    #[must_use]
    pub fn booked_memory_gib(&self) -> u64 {
        let usable = self
            .memory_bytes
            .saturating_sub(self.slots.saturating_mul(HARD_CAP_PAD));
        usable / 3 * 2 / GIB
    }

    /// The hard memory cap of a Mac job the farm booked `booking` bytes for: what the job needs.
    #[must_use]
    pub fn hard_memory(booking: u64) -> u64 {
        booking.saturating_mul(3) / 2 + HARD_CAP_PAD
    }

    /// The wall limit of a job of an action of `timeout`: the action's own, and nothing cuts
    /// it shorter, so a job stopped at it is the action's timeout.
    #[must_use]
    pub fn wall(&self, timeout: Duration) -> Duration {
        timeout + self.run_margin
    }

    /// The `Hello` of a virtual worker of `os`: what a real node of the shape would report, and
    /// no more. A Linux worker's drivers are [`DRIVERS`]: `action` and `whole_machine` leases
    /// are placed on it, a VM lease is not. A Mac worker reports `os=macos`, `arch=arm64` and the
    /// one driver `native`: only `action` leases are placed on it.
    pub(super) fn hello(
        &self,
        os: WorkerOs,
        node: &str,
        instance: &str,
        predecessor: &str,
    ) -> Hello {
        let entry = |key: &str, value: String| Capability {
            key: key.to_owned(),
            value,
        };
        let (arch, drivers, system): (&str, &[&str], &str) = match os {
            WorkerOs::Linux => (&self.arch, &DRIVERS, "linux"),
            WorkerOs::Mac => (MAC_ARCH, &[MAC_DRIVER], MAC_OS),
        };
        let mut capabilities = vec![
            entry("action_slots", self.slots.to_string()),
            entry("arch", arch.to_owned()),
            entry("cpus", (self.cpu_millis / 1_000).to_string()),
        ];
        capabilities.extend(drivers.iter().map(|d| entry("drivers", (*d).to_owned())));
        capabilities.extend([
            entry("mem_gib", self.booked_memory_gib().to_string()),
            entry("os", system.to_owned()),
        ]);
        Hello {
            protocol_version: PROTOCOL_VERSION,
            node_id: node.to_owned(),
            daemon_version: "kbf-server executor".to_owned(),
            capabilities,
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
