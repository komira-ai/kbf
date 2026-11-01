//! The records history keeps, and the values of its `ui/` index rows, as protobuf
//! messages.
//!
//! Every record carries the `instance` it was asked under, so a read filters by the
//! instances its caller may see. Field numbers are the wire format: a field is added
//! under a new number and never renumbered.

pub use crate::flog::{
    FLOG_STREAM_BYTES, FlogOmitted, FlogRecord, FlogStream, FlogSummary, ObjRow, flog_key,
};
use crate::run_id::RunId;

/// The longest tail kept of one log stream, after redaction.
pub const TAIL_BYTES: usize = 16 * 1024;

/// The longest command rendering kept, all of its strings together: the 4 KiB the
/// design sizes the failed-run section on (ui-data.md 5.3).
pub const COMMAND_BYTES: usize = 4 * 1024;

/// A digest on the wire: the SHA-256 and the size in bytes.
#[derive(Clone, PartialEq, Eq, Hash, prost::Message)]
pub struct WireDigest {
    /// The 32 hash bytes.
    #[prost(bytes = "vec", tag = "1")]
    pub hash: Vec<u8>,
    /// The size of the content.
    #[prost(uint64, tag = "2")]
    pub size: u64,
}

impl WireDigest {
    /// The digest of `size` bytes whose SHA-256 is `hash`.
    #[must_use]
    pub fn new(hash: [u8; 32], size: u64) -> WireDigest {
        WireDigest {
            hash: hash.to_vec(),
            size,
        }
    }

    /// The first 8 hash bytes as a number: the "digest16" (16 hex digits) the chunk's
    /// digest table is keyed by. A hash shorter than 8 bytes is padded with zeros.
    #[must_use]
    pub fn prefix16(&self) -> u64 {
        let mut b = [0; 8];
        let n = self.hash.len().min(8);
        b[..n].copy_from_slice(&self.hash[..n]);
        u64::from_be_bytes(b)
    }
}

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum RunOutcome {
    /// Not stated (an old or broken record).
    Unspecified = 0,
    /// The command exited 0.
    Ok = 1,
    /// The command exited non-zero.
    Failed = 2,
    /// The command ran past its timeout.
    TimedOut = 3,
    /// The run was killed for memory.
    MemoryKilled = 4,
    /// The farm failed the run (infrastructure).
    Infra = 5,
    /// The lease was lost.
    Lost = 6,
    /// The client cancelled.
    Cancelled = 7,
}

/// Which memory kill ended a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum MemoryKill {
    /// No memory kill.
    None = 0,
    /// The action passed its own limit.
    OwnLimit = 1,
    /// The node ran short and killed it.
    NodePressure = 2,
}

/// Which instrument measured a run's usage. A footprint peak and a cgroup peak count
/// different memory, so a reader keys them apart and never pools them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum UsageBasis {
    /// Not said: a record written before the field, or a run with no usage.
    Unspecified = 0,
    /// The lease's cgroup v2 counters (the `cgroup` block).
    Cgroup = 1,
    /// macOS physical footprint and per-uid CPU time (the `footprint` block).
    MacFootprint = 2,
}

/// The nine REAPI timestamps of a run plus the farm's grant, in microseconds after the
/// Unix epoch (0: not reported).
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct Timestamps {
    /// First submission, kept across requeues.
    #[prost(uint64, tag = "1")]
    pub queued_us: u64,
    /// The lease was granted.
    #[prost(uint64, tag = "2")]
    pub granted_us: u64,
    /// `worker_start_timestamp`.
    #[prost(uint64, tag = "3")]
    pub worker_start_us: u64,
    /// `input_fetch_start_timestamp`.
    #[prost(uint64, tag = "4")]
    pub input_fetch_start_us: u64,
    /// `input_fetch_completed_timestamp`.
    #[prost(uint64, tag = "5")]
    pub input_fetch_completed_us: u64,
    /// `execution_start_timestamp`.
    #[prost(uint64, tag = "6")]
    pub execution_start_us: u64,
    /// `execution_completed_timestamp`.
    #[prost(uint64, tag = "7")]
    pub execution_completed_us: u64,
    /// `output_upload_start_timestamp`.
    #[prost(uint64, tag = "8")]
    pub output_upload_start_us: u64,
    /// `output_upload_completed_timestamp`.
    #[prost(uint64, tag = "9")]
    pub output_upload_completed_us: u64,
    /// `worker_completed_timestamp`.
    #[prost(uint64, tag = "10")]
    pub worker_completed_us: u64,
}

/// The cgroup block the daemon reports for a run.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct Cgroup {
    /// CPU time, microseconds.
    #[prost(uint64, tag = "1")]
    pub cpu_usage_us: u64,
    /// User CPU time, microseconds.
    #[prost(uint64, tag = "2")]
    pub cpu_user_us: u64,
    /// System CPU time, microseconds.
    #[prost(uint64, tag = "3")]
    pub cpu_system_us: u64,
    /// CFS periods.
    #[prost(uint64, tag = "4")]
    pub periods: u64,
    /// Periods throttled.
    #[prost(uint64, tag = "5")]
    pub throttled_periods: u64,
    /// Time throttled, microseconds.
    #[prost(uint64, tag = "6")]
    pub throttled_us: u64,
    /// Peak memory, bytes (`memory.peak`). `None` when the kernel has no such file:
    /// absent is not zero, and a zero peak is not a memory sample. A record written
    /// before this field was optional reads 0 as `None`.
    #[prost(uint64, optional, tag = "7")]
    pub memory_peak_bytes: Option<u64>,
    /// Memory per NUMA node, bytes, by node number.
    #[prost(uint64, repeated, tag = "8")]
    pub numa_bytes: Vec<u64>,
    /// Wall time from the action's start to its exit, microseconds. `None` when the
    /// runtime reported none (and for the first chunks, written before it existed).
    #[prost(uint64, optional, tag = "9")]
    pub wall_us: Option<u64>,
    /// `memory.events` `max` count: above zero, the peak is a floor, not the need.
    #[prost(uint64, optional, tag = "10")]
    pub memory_events_max: Option<u64>,
    /// `memory.events` `oom_kill` count.
    #[prost(uint64, optional, tag = "11")]
    pub memory_events_oom_kill: Option<u64>,
    /// `memory.swap.peak`, bytes; `None` on a kernel without it.
    #[prost(uint64, optional, tag = "12")]
    pub memory_swap_peak_bytes: Option<u64>,
    /// The largest anon + swap of any in-run sample, bytes.
    #[prost(uint64, optional, tag = "13")]
    pub memory_anon_swap_peak_bytes: Option<u64>,
    /// The lease's CPU stall total (`cpu.pressure` some), microseconds.
    #[prost(uint64, optional, tag = "14")]
    pub cpu_pressure_some_us: Option<u64>,
    /// The lease's memory stall totals (`memory.pressure` some and full), microseconds.
    #[prost(uint64, optional, tag = "15")]
    pub memory_pressure_some_us: Option<u64>,
    /// See [`Cgroup::memory_pressure_some_us`].
    #[prost(uint64, optional, tag = "16")]
    pub memory_pressure_full_us: Option<u64>,
    /// The CPUs the lease's cgroup could run on.
    #[prost(uint32, optional, tag = "17")]
    pub cpus_effective: Option<u32>,
    /// The most CPU any one window of node-load readings saw the lease use, thousandths of a
    /// CPU; `None` when the lease had no full window.
    #[prost(uint64, optional, tag = "18")]
    pub peak_window_millicores: Option<u64>,
}

/// The footprint block the macOS native driver reports for a run. A field that is
/// `None` was not readable: absent is not zero.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct Footprint {
    /// The highest physical footprint of the run's process tree, bytes. Not comparable
    /// with [`Cgroup::memory_peak_bytes`].
    #[prost(uint64, optional, tag = "1")]
    pub peak_footprint_bytes: Option<u64>,
    /// User CPU time of the lease's uid, microseconds.
    #[prost(uint64, optional, tag = "2")]
    pub cpu_user_us: Option<u64>,
    /// System CPU time of the lease's uid, microseconds.
    #[prost(uint64, optional, tag = "3")]
    pub cpu_system_us: Option<u64>,
    /// Wall time from the action's start to its exit, microseconds.
    #[prost(uint64, optional, tag = "4")]
    pub wall_us: Option<u64>,
}

/// One output of a run.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct Output {
    /// The path the action named.
    #[prost(string, tag = "1")]
    pub path: String,
    /// The file's digest, or the directory's tree digest.
    #[prost(message, optional, tag = "2")]
    pub digest: Option<WireDigest>,
    /// A directory, not a file.
    #[prost(bool, tag = "3")]
    pub is_dir: bool,
}

/// One attempt that ran (or was granted) on a node: the run record.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct RunRecord {
    /// The run id's commit time, milliseconds.
    #[prost(uint64, tag = "1")]
    pub time_ms: u64,
    /// The run id's stream.
    #[prost(uint32, tag = "2")]
    pub stream: u32,
    /// The run id's sequence in its stream.
    #[prost(uint32, tag = "3")]
    pub seq: u32,
    /// The REAPI instance name the run was asked under.
    #[prost(string, tag = "4")]
    pub instance: String,
    /// The action digest.
    #[prost(message, optional, tag = "5")]
    pub action: Option<WireDigest>,
    /// The ActionResult blob, when one was stored.
    #[prost(message, optional, tag = "6")]
    pub result: Option<WireDigest>,
    /// The Command blob.
    #[prost(message, optional, tag = "7")]
    pub command: Option<WireDigest>,
    /// The input root.
    #[prost(message, optional, tag = "8")]
    pub input_root: Option<WireDigest>,
    /// stdout, when the run has logs.
    #[prost(message, optional, tag = "9")]
    pub stdout: Option<WireDigest>,
    /// stderr, when the run has logs.
    #[prost(message, optional, tag = "10")]
    pub stderr: Option<WireDigest>,
    /// The 16-hex shape key.
    #[prost(fixed64, tag = "11")]
    pub shape: u64,
    /// The configuration hash taken out of the output paths (`-` when none).
    #[prost(string, tag = "12")]
    pub cfg: String,
    /// The node that ran it.
    #[prost(string, tag = "13")]
    pub node: String,
    /// The lease's epoch (term).
    #[prost(uint64, tag = "14")]
    pub lease_epoch: u64,
    /// The lease's sequence.
    #[prost(uint64, tag = "15")]
    pub lease_seq: u64,
    /// The command's exit code.
    #[prost(int32, tag = "16")]
    pub exit_code: i32,
    /// How the run ended.
    #[prost(enumeration = "RunOutcome", tag = "17")]
    pub outcome: i32,
    /// The memory kill that ended it, if any.
    #[prost(enumeration = "MemoryKill", tag = "18")]
    pub memory_kill: i32,
    /// The memory booked for the attempt, bytes: the grant its `Start` carried (a record
    /// written before the grant was kept holds it for a memory kill only).
    #[prost(uint64, tag = "19")]
    pub booked_memory_bytes: u64,
    /// The booking the next attempt gets after a memory kill, bytes.
    #[prost(uint64, tag = "20")]
    pub raised_memory_bytes: u64,
    /// The action's own limit at an own-limit kill, bytes.
    #[prost(uint64, tag = "21")]
    pub cap_bytes: u64,
    /// The last reason the scheduler gave while the action waited.
    #[prost(string, tag = "22")]
    pub wait_reason: String,
    /// Platform, size class and QoS of the queue it waited in.
    #[prost(string, tag = "23")]
    pub partition: String,
    /// Farm reruns used.
    #[prost(uint32, tag = "24")]
    pub farm_runs: u32,
    /// Farm reruns allowed.
    #[prost(uint32, tag = "25")]
    pub farm_budget: u32,
    /// Server time before the offer (the missing-inputs walk), microseconds.
    #[prost(uint64, tag = "26")]
    pub presubmit_us: u64,
    /// The timestamps.
    #[prost(message, optional, tag = "27")]
    pub timestamps: Option<Timestamps>,
    /// The cgroup block.
    #[prost(message, optional, tag = "28")]
    pub cgroup: Option<Cgroup>,
    /// The outputs listing.
    #[prost(message, repeated, tag = "29")]
    pub outputs: Vec<Output>,
    /// The daemon's reason text for an infrastructure end (at most 1 KiB).
    #[prost(string, tag = "30")]
    pub reason: String,
    /// The memory the run used when it was killed, bytes.
    #[prost(uint64, tag = "31")]
    pub memory_used_bytes: u64,
    /// The memory the run booked, bytes, when it was killed at its own limit (zero
    /// otherwise). This is the booking, not the cgroup's `memory.max`, which is 1.5 times
    /// the booking plus 512 MiB.
    #[prost(uint64, tag = "32")]
    pub memory_limit_bytes: u64,
    /// The action is a test run (its shape names a test execution root). Added after
    /// the first chunks were written: a record without it reads `false`.
    #[prost(bool, tag = "33")]
    pub test: bool,
    /// The target label the run's shape carried (Bazel names it); empty when it had
    /// none. Added after the first chunks were written: a record without it reads empty.
    #[prost(string, tag = "34")]
    pub target: String,
    /// When the operation was queued for this attempt (submitted, or queued again after
    /// the attempt before it), microseconds; 0 when not kept. `timestamps.queued_us`
    /// is the first submission, so it cannot give this. Added after the first chunks
    /// were written.
    #[prost(uint64, tag = "35")]
    pub attempt_queued_us: u64,
    /// When the attempt's worker was sent the Start, microseconds; 0: never.
    #[prost(uint64, tag = "36")]
    pub started_us: u64,
    /// When the attempt ended, microseconds; 0: it had not.
    #[prost(uint64, tag = "37")]
    pub ended_us: u64,
    /// What kind of owner `target` names: 0 none or unknown, 1 a rule's target, 2 an
    /// anonymous target, 3 a BXL function.
    #[prost(uint32, tag = "38")]
    pub target_kind: u32,
    /// Which instrument measured the run: `cgroup` for the `cgroup` block, `footprint`
    /// for the `footprint` block. Added after the first chunks were written: a record
    /// without it reads unspecified, and a reader takes a `cgroup` block as cgroup.
    #[prost(enumeration = "UsageBasis", tag = "39")]
    pub basis: i32,
    /// The footprint block, for a run measured on macOS. Present only with the
    /// `footprint` basis; the `cgroup` block is then absent.
    #[prost(message, optional, tag = "40")]
    pub footprint: Option<Footprint>,
    /// The node's CPU pressure when the run started (`some avg10` of
    /// `/proc/pressure/cpu`), in hundredths of a percent; absent when the daemon did
    /// not read it. Absent is not zero.
    #[prost(uint32, optional, tag = "41")]
    pub node_cpu_pressure_centi: Option<u32>,
    /// The CPU booked for the lease, in thousandths of a CPU: the grant the server sent in
    /// the `Start`, else the daemon's report; absent for a lease the server never
    /// started (taken up from the state store) whose daemon reported none.
    #[prost(uint64, optional, tag = "42")]
    pub booked_millicpus: Option<u64>,
    /// The scheduler's operation the attempt belonged to: every attempt of one operation
    /// (a ladder of memory bookings) carries the same one, so they can be regrouped.
    /// Absent: a record written before the field, or a row that named none. Fields 43 on
    /// follow the estimator's usage fields (39 to 42).
    #[prost(uint64, optional, tag = "43")]
    pub operation_id: Option<u64>,
    /// Where the booked cores came from.
    #[prost(enumeration = "BookSource", tag = "44")]
    pub cpu_source: i32,
    /// Where the booked memory came from.
    #[prost(enumeration = "BookSource", tag = "45")]
    pub memory_source: i32,
    /// The summed size of the distinct files of the input tree, bytes. Absent, not 0:
    /// the front's walk did not total it (it skipped a verified subtree, or never ran).
    #[prost(uint64, optional, tag = "46")]
    pub input_bytes: Option<u64>,
    /// The number of distinct files of the input tree; absent as `input_bytes` is.
    #[prost(uint64, optional, tag = "47")]
    pub input_files: Option<u64>,
    /// Where the three phase durations of `timestamps` came from. `Unspecified` is a record
    /// written before the field, which reads as measured. Field 50: fields 48 and 49 are taken
    /// by the invocation ids (#2200).
    #[prost(enumeration = "PhaseTimes", tag = "50")]
    pub phase_times: i32,
}

/// Where a run record's `input_fetch`, `execution` and `output_upload` times came from. For
/// a `TaskClaim` only the three durations are data: never the instants, or the gaps between
/// them and the server's `worker_start` and `worker_completed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum PhaseTimes {
    /// Not recorded: a record written before the field. Read as measured, which is what
    /// every such record was (a daemon's run, or an executor run with no phase times).
    Unspecified = 0,
    /// A daemon measured them.
    Measured = 1,
    /// A task claimed them, laid out ending at the frontend's read of its pack.
    TaskClaim = 2,
}

impl PhaseTimes {
    /// The name the read API writes: an unrecorded record is `measured`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Unspecified | Self::Measured => "measured",
            Self::TaskClaim => "task_claim",
        }
    }
}

/// Where one axis of an attempt's booking came from. A record written before the field
/// reads `Unspecified`: nobody recorded it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum BookSource {
    /// Not recorded.
    Unspecified = 0,
    /// The client named no size: the farm's default booked it.
    Default = 1,
    /// The client named a size, whatever it was (a size equal to the default included).
    Hint = 2,
}

impl RunRecord {
    /// The run's id.
    #[must_use]
    pub fn run_id(&self) -> RunId {
        RunId::new(self.time_ms, crate::run_id::StreamId(self.stream), self.seq)
    }

    /// Sets the id fields from `id`.
    pub fn set_run_id(&mut self, id: RunId) {
        self.time_ms = id.time_ms();
        self.stream = id.stream().0;
        self.seq = id.seq();
    }

    /// Whether the run failed (anything but [`RunOutcome::Ok`]).
    #[must_use]
    pub fn failed(&self) -> bool {
        self.outcome() != RunOutcome::Ok
    }

    /// Execution time (execution start to completion), microseconds; 0 when either
    /// timestamp is missing.
    #[must_use]
    pub fn execution_us(&self) -> u64 {
        self.timestamps.as_ref().map_or(0, |t| {
            t.execution_completed_us
                .saturating_sub(t.execution_start_us)
        })
    }

    /// Wall time (first queued to worker completed), microseconds; 0 when either
    /// timestamp is missing.
    #[must_use]
    pub fn wall_us(&self) -> u64 {
        self.timestamps
            .as_ref()
            .map_or(0, |t| t.worker_completed_us.saturating_sub(t.queued_us))
    }
}

/// How a cache hit was served.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum HitHow {
    /// Not stated.
    Unspecified = 0,
    /// `GetActionResult` found it.
    CacheHit = 1,
    /// `Execute` found it in the action cache.
    CacheHitExecute = 2,
}

/// A cache hit: no run, but the build used the result.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct HitRecord {
    /// When the answer was sent, milliseconds.
    #[prost(uint64, tag = "1")]
    pub time_ms: u64,
    /// The instance asked.
    #[prost(string, tag = "2")]
    pub instance: String,
    /// The action digest.
    #[prost(message, optional, tag = "3")]
    pub action: Option<WireDigest>,
    /// How it was served.
    #[prost(enumeration = "HitHow", tag = "4")]
    pub how: i32,
    /// Its position among the chunk's hits (set by the writer).
    #[prost(uint32, tag = "5")]
    pub row: u32,
}

/// Why a tail is missing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum TailOmitted {
    /// The tail is present (possibly empty).
    No = 0,
    /// The log was too large to read for a tail.
    TooLarge = 1,
    /// The log could not be read.
    Unreadable = 2,
    /// The read budget was not free in time (the read is not retried: the tail is left
    /// out rather than the run held).
    Busy = 3,
}

/// The last bytes of one log stream of a failed run, after redaction.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct Tail {
    /// At most [`TAIL_BYTES`] bytes: the end of the log.
    #[prost(bytes = "vec", tag = "1")]
    pub bytes: Vec<u8>,
    /// The whole log's length.
    #[prost(uint64, tag = "2")]
    pub total_bytes: u64,
    /// Why there is no tail, if there is none.
    #[prost(enumeration = "TailOmitted", tag = "3")]
    pub omitted: i32,
}

impl Tail {
    /// The tail of `log`: its last [`TAIL_BYTES`] bytes.
    #[must_use]
    pub fn of(log: &[u8]) -> Tail {
        let from = log.len().saturating_sub(TAIL_BYTES);
        Tail {
            bytes: log[from..].to_vec(),
            total_bytes: log.len() as u64,
            omitted: TailOmitted::No.into(),
        }
    }
}

/// A decoded, redacted rendering of a run's Command: arguments as sent after redaction,
/// environment variable names only (never values).
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct CommandRendering {
    /// The arguments.
    #[prost(string, repeated, tag = "1")]
    pub argv: Vec<String>,
    /// The environment variable names.
    #[prost(string, repeated, tag = "2")]
    pub env_names: Vec<String>,
    /// The working directory.
    #[prost(string, tag = "3")]
    pub working_directory: String,
    /// The output paths.
    #[prost(string, repeated, tag = "4")]
    pub output_paths: Vec<String>,
    /// Strings were dropped to keep the rendering within [`COMMAND_BYTES`].
    #[prost(bool, tag = "5")]
    pub truncated: bool,
}

impl CommandRendering {
    /// Drops strings from the end (output paths, then environment names, then
    /// arguments) until the rendering's strings fit [`COMMAND_BYTES`], and marks it
    /// truncated if any were dropped.
    pub fn bound(&mut self) {
        let mut size: usize = self.working_directory.len()
            + self.argv.iter().map(String::len).sum::<usize>()
            + self.env_names.iter().map(String::len).sum::<usize>()
            + self.output_paths.iter().map(String::len).sum::<usize>();
        for list in [&mut self.output_paths, &mut self.env_names, &mut self.argv] {
            while size > COMMAND_BYTES {
                let Some(s) = list.pop() else { break };
                size -= s.len();
                self.truncated = true;
            }
        }
        if size > COMMAND_BYTES {
            self.working_directory.clear();
            self.truncated = true;
        }
    }
}

/// The failed-run section of one run: its tails and command rendering, kept in its
/// own section of the chunk so a list of failures reads only those blocks.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct FailedRun {
    /// The run's sequence in its stream (the chunk holds one stream).
    #[prost(uint32, tag = "1")]
    pub seq: u32,
    /// The instance the run was asked under.
    #[prost(string, tag = "2")]
    pub instance: String,
    /// The stdout tail.
    #[prost(message, optional, tag = "3")]
    pub stdout: Option<Tail>,
    /// The stderr tail.
    #[prost(message, optional, tag = "4")]
    pub stderr: Option<Tail>,
    /// The command rendering.
    #[prost(message, optional, tag = "5")]
    pub command: Option<CommandRendering>,
    /// Why some log of the run was not copied into its failure-log object.
    #[prost(enumeration = "FlogOmitted", tag = "6")]
    pub flog_omitted: i32,
}

/// A test outcome's status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum TestStatus {
    /// Not stated.
    Unspecified = 0,
    /// Passed.
    Pass = 1,
    /// Failed.
    Fail = 2,
    /// Timed out.
    Timeout = 3,
    /// Killed at its own memory limit.
    OomKilled = 4,
    /// The farm failed it.
    Infra = 5,
}

/// One run of a test shape: what the 400-day outcome history keeps after the run record
/// is gone. Its key is (instance, shape, cfg, branch, time).
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct OutcomeRecord {
    /// The instance.
    #[prost(string, tag = "1")]
    pub instance: String,
    /// The shape key.
    #[prost(fixed64, tag = "2")]
    pub shape: u64,
    /// The configuration hash.
    #[prost(string, tag = "3")]
    pub cfg: String,
    /// The branch, when a report or header named it.
    #[prost(string, tag = "4")]
    pub branch: String,
    /// The run's commit time, milliseconds.
    #[prost(uint64, tag = "5")]
    pub time_ms: u64,
    /// The run id's 12 bytes.
    #[prost(bytes = "vec", tag = "6")]
    pub run_id: Vec<u8>,
    /// The action digest (the flake proof: same digest, different status).
    #[prost(message, optional, tag = "7")]
    pub action: Option<WireDigest>,
    /// The commit, when the farm knows it.
    #[prost(string, tag = "8")]
    pub commit: String,
    /// The status.
    #[prost(enumeration = "TestStatus", tag = "9")]
    pub status: i32,
    /// The invocation key; empty for an id-less run.
    #[prost(string, tag = "10")]
    pub inv_key: String,
}

/// One record of an invocation chunk.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct ChunkRecord {
    /// The record.
    #[prost(oneof = "ChunkBody", tags = "1, 2, 3")]
    pub body: Option<ChunkBody>,
}

/// What a [`ChunkRecord`] holds.
// A run record is larger than a hit; these are transient decode values, read a block
// at a time, so boxing would cost an allocation per record for no memory saved.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, PartialEq, Eq, prost::Oneof)]
pub enum ChunkBody {
    /// A run.
    #[prost(message, tag = "1")]
    Run(RunRecord),
    /// A failed run's section.
    #[prost(message, tag = "2")]
    Failed(FailedRun),
    /// A hit.
    #[prost(message, tag = "3")]
    Hit(HitRecord),
}

/// The section byte of a run's key in a chunk.
pub const SECTION_RUN: u8 = 0;
/// The section byte of a failed-run section's key.
pub const SECTION_FAILED: u8 = 1;
/// The section byte of a hit's key.
pub const SECTION_HIT: u8 = 2;

/// The sort key of a record in a chunk: its section byte, then the run's sequence (or
/// the hit's row), big-endian. Blocks are cut in key order, so the block index finds
/// a run by its sequence and the failed-run section is a run of adjacent blocks.
#[must_use]
pub fn chunk_key(section: u8, n: u32) -> [u8; 5] {
    let mut k = [section, 0, 0, 0, 0];
    k[1..].copy_from_slice(&n.to_be_bytes());
    k
}

impl ChunkBody {
    /// The record's sort key.
    #[must_use]
    pub fn key(&self) -> [u8; 5] {
        match self {
            ChunkBody::Run(r) => chunk_key(SECTION_RUN, r.seq),
            ChunkBody::Failed(f) => chunk_key(SECTION_FAILED, f.seq),
            ChunkBody::Hit(h) => chunk_key(SECTION_HIT, h.row),
        }
    }

    /// The instance the record was asked under.
    #[must_use]
    pub fn instance(&self) -> &str {
        match self {
            ChunkBody::Run(r) => &r.instance,
            ChunkBody::Failed(f) => &f.instance,
            ChunkBody::Hit(h) => &h.instance,
        }
    }
}

/// Whether an invocation is still recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum InvState {
    /// Records may still come.
    Open = 0,
    /// The writer flushed its last chunk after the invocation turned idle.
    Closed = 1,
}

/// Instances an [`InvocationHeader`] lists at most. A build uses one or a few; the
/// bound keeps a header row (and the reader's all-must-allow check) small whatever
/// names a client sends.
pub const MAX_INSTANCES: usize = 64;

/// A `ui/invid/<inv-key>/<writer>` row: one writer's header of an invocation, counting the
/// records that writer wrote (a reader merges the rows of every writer). The newest chunk's
/// footer carries the same header, so the row can be restored from the object.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct InvocationHeader {
    /// The client's invocation id; empty for an id-less pseudo-invocation.
    #[prost(string, tag = "1")]
    pub record_id: String,
    /// An id-less pseudo-invocation (listed nowhere).
    #[prost(bool, tag = "2")]
    pub idless: bool,
    /// The instances the invocation's records used, sorted: the first
    /// [`MAX_INSTANCES`] by name. The names are the client's, so the list is bounded.
    #[prost(string, repeated, tag = "3")]
    pub instances: Vec<String>,
    /// Run records written.
    #[prost(uint64, tag = "4")]
    pub runs: u64,
    /// Hit records written.
    #[prost(uint64, tag = "5")]
    pub hits: u64,
    /// Failed runs written.
    #[prost(uint64, tag = "6")]
    pub failed: u64,
    /// The first record's time, milliseconds.
    #[prost(uint64, tag = "7")]
    pub first_seen_ms: u64,
    /// The last record's time, milliseconds.
    #[prost(uint64, tag = "8")]
    pub last_seen_ms: u64,
    /// When the last chunk was written: the gap starts here when the invocation was
    /// left open by a crash.
    #[prost(uint64, tag = "9")]
    pub last_chunk_ms: u64,
    /// Chunks written.
    #[prost(uint32, tag = "10")]
    pub chunks: u32,
    /// Open or closed.
    #[prost(enumeration = "InvState", tag = "11")]
    pub state: i32,
    /// Records the writer dropped (full buffers): the history has a hole.
    #[prost(uint64, tag = "12")]
    pub dropped: u64,
    /// The records used more than [`MAX_INSTANCES`] instances, so `instances` does not
    /// list them all. A reader cannot check every instance, so it answers no caller.
    #[prost(bool, tag = "13")]
    pub instances_truncated: bool,
    /// The time the invocation's `ui/inv` list entries are keyed by: its first record's
    /// time when the writer first listed it, never moved, so a list has one entry per
    /// instance. An entry whose key time is not this one is stale and is not shown.
    #[prost(uint64, tag = "14")]
    pub list_ms: u64,
}

/// The `ui/ichunk/<inv-key>/<writer>/<seq>` row: where one chunk is and what it holds.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct ChunkRow {
    /// The `yyyymmdd` day in the object's key.
    #[prost(uint32, tag = "1")]
    pub day: u32,
    /// The object's length.
    #[prost(uint64, tag = "2")]
    pub bytes: u64,
    /// The stream of its runs (meaningful only when it has runs).
    #[prost(uint32, tag = "3")]
    pub stream: u32,
    /// Its first run's sequence.
    #[prost(uint32, tag = "4")]
    pub first_seq: u32,
    /// Its last run's sequence.
    #[prost(uint32, tag = "5")]
    pub last_seq: u32,
    /// Run records in it.
    #[prost(uint32, tag = "6")]
    pub runs: u32,
    /// Hit records in it.
    #[prost(uint32, tag = "7")]
    pub hits: u32,
    /// Failed runs in it.
    #[prost(uint32, tag = "8")]
    pub failed: u32,
    /// The earliest record time, milliseconds.
    #[prost(uint64, tag = "9")]
    pub first_ms: u64,
    /// The latest record time, milliseconds.
    #[prost(uint64, tag = "10")]
    pub last_ms: u64,
    /// Where the footer (with the trailer) starts.
    #[prost(uint64, tag = "11")]
    pub footer_offset: u64,
    /// The footer's length with the trailer.
    #[prost(uint32, tag = "12")]
    pub footer_len: u32,
}

impl ChunkRow {
    /// Whether the chunk holds run `seq` of `stream`.
    #[must_use]
    pub fn holds(&self, stream: u32, seq: u32) -> bool {
        self.runs > 0 && self.stream == stream && (self.first_seq..=self.last_seq).contains(&seq)
    }
}

/// The `ui/stream/<day>/<stream>/<first ms>` row: which invocation and writer a stream
/// belongs to.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct StreamRow {
    /// The invocation key.
    #[prost(string, tag = "1")]
    pub inv_key: String,
    /// The writer.
    #[prost(uint64, tag = "2")]
    pub writer: u64,
}

/// A run among the slowest of an invocation.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct Slow {
    /// The run id's 12 bytes.
    #[prost(bytes = "vec", tag = "1")]
    pub run_id: Vec<u8>,
    /// The time it is ranked by, microseconds.
    #[prost(uint64, tag = "2")]
    pub us: u64,
}

/// An entry of a chunk's digest table.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct DigestRow {
    /// The digest's first 8 hash bytes.
    #[prost(fixed64, tag = "1")]
    pub prefix: u64,
    /// The record's key in the chunk.
    #[prost(bytes = "vec", tag = "2")]
    pub key: Vec<u8>,
}

/// The family summary of an invocation chunk's footer: everything its index rows hold,
/// so a row lost after the PUT is restored from the object with no LIST, and the
/// invocation's cumulative summary up to this chunk.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct ChunkSummary {
    /// The invocation key.
    #[prost(string, tag = "1")]
    pub inv_key: String,
    /// The writer.
    #[prost(uint64, tag = "2")]
    pub writer: u64,
    /// The chunk's sequence among every chunk its writer wrote in that life (one count
    /// across invocations, so a writer never makes a key twice).
    #[prost(uint32, tag = "3")]
    pub seq: u32,
    /// What the chunk's `ichunk` row holds, but for the object's length and footer
    /// position, which are zero here: the footer cannot hold its own position. A
    /// restore takes them from the footer range it read.
    #[prost(message, optional, tag = "4")]
    pub row: Option<ChunkRow>,
    /// The open time of the chunk's stream (its first id's time), the `ui/stream` row's
    /// key.
    #[prost(uint64, tag = "5")]
    pub stream_first_ms: u64,
    /// The invocation's header as of this chunk.
    #[prost(message, optional, tag = "6")]
    pub header: Option<InvocationHeader>,
    /// The invocation's slowest runs by execution time so far, slowest first.
    #[prost(message, repeated, tag = "7")]
    pub slowest_execution: Vec<Slow>,
    /// The invocation's slowest runs by wall time so far, slowest first.
    #[prost(message, repeated, tag = "8")]
    pub slowest_wall: Vec<Slow>,
    /// This chunk's action digests to record keys, sorted by prefix.
    #[prost(message, repeated, tag = "9")]
    pub digests: Vec<DigestRow>,
}

#[cfg(test)]
#[path = "records_tests.rs"]
mod tests;
