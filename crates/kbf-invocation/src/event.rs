//! The events a record is built from (section 3.4): the calls a client makes, and what
//! the farm does for the operations those calls queued or joined. Each carries the
//! server's wall time as a value; nothing here reads a clock.

use kbf_sched::RequeueReason;
use kbf_types::{Digest, Failure, LeaseId, OperationId, WorkerId};

/// A wall-clock time, in microseconds since the Unix epoch, read by the caller.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WallTime(u64);

impl WallTime {
    /// The time `micros` microseconds after the Unix epoch.
    #[must_use]
    pub const fn from_unix_micros(micros: u64) -> Self {
        Self(micros)
    }

    /// Microseconds since the Unix epoch.
    #[must_use]
    pub const fn unix_micros(self) -> u64 {
        self.0
    }
}

/// An invocation id as the client sent it (`tool_invocation_id`), or one the server
/// minted for an Execute that sent none (section 2.3).
///
/// The metadata layer accepts an id only in the character set of section 2.2, which
/// has no `~`; this type does not check it again.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InvocationId(String);

impl InvocationId {
    /// The id `id`, already accepted by the metadata layer.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id's text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Who a call came from: the caller's `public` authentication metadata as JSON text,
/// empty when it has none. Two calls have the same owner exactly when the texts are
/// equal (section 2.4).
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Owner(pub String);

/// One event, in the order the recorder takes them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A call that carried an invocation id.
    Call(Box<Call>),
    /// The Command digest the recorder read from an Action blob: set on that action's
    /// row in the invocation the id and owner name.
    Command {
        /// The invocation.
        id: InvocationId,
        /// The caller that made the action's call.
        owner: Owner,
        /// The Action digest.
        action: Digest,
        /// The Command digest the Action names.
        command: Digest,
    },
    /// A lease of `operation` was granted on `worker`.
    Granted {
        /// The scheduler's operation.
        operation: OperationId,
        /// The lease.
        lease: LeaseId,
        /// The worker (node id).
        worker: WorkerId,
        /// When.
        at: WallTime,
    },
    /// The worker holding `lease` was sent its Start.
    Started {
        /// The scheduler's operation.
        operation: OperationId,
        /// The lease.
        lease: LeaseId,
        /// When.
        at: WallTime,
    },
    /// `lease` was given up and `operation` queued again.
    Requeued {
        /// The scheduler's operation.
        operation: OperationId,
        /// The lease given up.
        lease: LeaseId,
        /// Why.
        reason: RequeueReason,
        /// When.
        at: WallTime,
    },
    /// `operation` ended, and its callers were answered with `outcome`.
    Finished(Finished),
}

/// An operation's end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finished {
    /// The scheduler's operation.
    pub operation: OperationId,
    /// The lease whose report ended it, and how; `None` when no lease did (a refusal).
    pub lease: Option<(LeaseId, LeaseEnd)>,
    /// What the callers were answered.
    pub outcome: Outcome,
    /// The stored ActionResult and what it lists, when there is one.
    pub result: Option<Listing>,
    /// When.
    pub at: WallTime,
}

/// How the lease that ended an operation ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeaseEnd {
    /// Its result was accepted.
    Accepted,
    /// It reported `failure`, with the daemon's reason text.
    Failed(Failure, String),
}

/// What a caller was answered (section 3.2, `outcome`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// An OK status: the action exited with `exit_code`.
    Ok {
        /// The action's exit code.
        exit_code: i32,
    },
    /// A non-OK status other than a timeout.
    Failed {
        /// The gRPC status code.
        code: i32,
        /// The status message.
        message: String,
    },
    /// The action ran past its timeout (DEADLINE_EXCEEDED).
    TimedOut,
    /// The farm refused to run it.
    Refused(String),
}

impl Outcome {
    /// Whether the client sees a failed action: anything but an OK status with exit 0.
    #[must_use]
    pub const fn is_failure(&self) -> bool {
        !matches!(self, Self::Ok { exit_code: 0 })
    }
}

/// A call that carried an invocation id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    /// The invocation id.
    pub id: InvocationId,
    /// Whether the server minted `id` (section 2.3).
    pub minted: bool,
    /// The caller.
    pub owner: Owner,
    /// The REAPI instance name.
    pub instance: String,
    /// When the call was seen.
    pub at: WallTime,
    /// What the call's `RequestMetadata` says beside the id.
    pub metadata: Metadata,
    /// What the call was and how it was answered.
    pub kind: CallKind,
}

/// The `RequestMetadata` fields of section 2.1 other than the invocation id; `None`
/// (or empty) when not sent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Metadata {
    /// `tool_details.tool_name`.
    pub tool: Option<String>,
    /// `tool_details.tool_version`.
    pub tool_version: Option<String>,
    /// `correlated_invocations_id`.
    pub correlated_invocations_id: Option<String>,
    /// `target_id`.
    pub target: Option<String>,
    /// `action_mnemonic`.
    pub mnemonic: Option<String>,
    /// `configuration_id`.
    pub configuration: Option<String>,
    /// `action_id`.
    pub action_id: Option<String>,
}

/// A call, by what the client asked and what it was answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallKind {
    /// Execute of `action`.
    Execute {
        /// The Action digest.
        action: Digest,
        /// How the front answered it.
        answer: ExecuteAnswer,
    },
    /// GetActionResult of `action`: the cached result, or `None` for a miss.
    GetActionResult {
        /// The Action digest.
        action: Digest,
        /// The result the action cache holds for it.
        hit: Option<Listing>,
    },
    /// FindMissingBlobs.
    FindMissingBlobs {
        /// The digests asked about.
        asked: u64,
        /// Those answered missing.
        missing: u64,
    },
    /// CAS or ByteStream bytes the client sent.
    Uploaded(u64),
    /// CAS or ByteStream bytes the client received.
    Downloaded(u64),
}

/// How the front answered an Execute.
///
/// `name` is the REAPI operation name the caller was given; the front names one per
/// caller, so twins joined to one scheduler operation have different names.
/// `operation` is the scheduler's, which the lease events name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecuteAnswer {
    /// From the action cache.
    Cached {
        /// The REAPI operation name (`operations/cached/...`).
        name: String,
        /// The cached result.
        result: Box<Listing>,
    },
    /// Attached to a twin's operation already in flight.
    Joined {
        /// The REAPI operation name.
        name: String,
        /// The scheduler's operation.
        operation: OperationId,
    },
    /// Queued as a new operation.
    Queued {
        /// The REAPI operation name.
        name: String,
        /// The scheduler's operation.
        operation: OperationId,
    },
}

/// An ActionResult's digest and what it lists: the part of a result a row copies
/// (section 3.2), so a page lists outputs and logs after the blob is gone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listing {
    /// The ActionResult blob.
    pub result: Digest,
    /// The action's exit code.
    pub exit_code: i32,
    /// Its output files and directories, in the result's order. A row keeps the first
    /// [`crate::OUTPUTS_PER_ROW`].
    pub outputs: Vec<Output>,
    /// Its stdout blob.
    pub stdout: Option<Digest>,
    /// Its stderr blob.
    pub stderr: Option<Digest>,
    /// What the action used, when the result carries a `ResourceUsage`.
    pub usage: Option<Usage>,
}

/// An output file, or an output directory as its tree digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    /// The output's path, relative to the action's working directory.
    pub path: String,
    /// The file's digest, or the directory's Tree digest.
    pub digest: Digest,
    /// Whether the file is executable (false for a directory).
    pub executable: bool,
    /// Whether this is a directory.
    pub directory: bool,
}

/// `kbf.worker.v1.ResourceUsage`: zero in a field means not measured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// User CPU time, in microseconds.
    pub cpu_user_micros: u64,
    /// System CPU time, in microseconds.
    pub cpu_system_micros: u64,
    /// The largest resident memory of one process, in bytes.
    pub peak_memory_bytes: u64,
    /// Wall time, in microseconds.
    pub wall_micros: u64,
}
