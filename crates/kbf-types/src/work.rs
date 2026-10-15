//! The vocabulary of scheduled work: operations, workers, waiters, the dedup key, the
//! request vector, fence policies, outcomes, and the records the scheduler commits to
//! the control log.
//!
//! These are shared because the front (which answers waiters), the server (which
//! carries out scheduler effects) and the daemon (which runs leases) all speak them.

use std::fmt;

use crate::{Digest, LeaseId, LeaseKind};

/// One operation: one execution of an action that one or more waiters are attached to.
/// Numbered by the scheduler in submission order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OperationId(pub u64);

impl fmt::Display for OperationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "op-{}", self.0)
    }
}

/// One client call waiting on an operation (an `Execute` or `WaitExecution` stream).
/// Numbered by the front; the scheduler only hands it back in [`Answer`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WaiterId(pub u64);

/// A worker node, by the name it registered with.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkerId(String);

impl WorkerId {
    /// The worker called `name`.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The worker's name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What makes two executions the same work: the REAPI instance name and the action
/// digest. In-flight dedup joins a second call to a running twin only when both match;
/// the same digest under another instance is different work (another namespace, maybe
/// another cache), so it never joins.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ActionKey {
    /// The REAPI instance name the client sent.
    pub instance: String,
    /// The digest of the `Action` message.
    pub action: Digest,
}

/// A request vector, or a node's capacity in the same units.
///
/// GPUs are whole and exclusive: a request books whole GPUs, and a booked GPU is
/// another lease's only once the lease holding it ends. VM slots are counted the same
/// way: a node's `vms` is how many VMs may run on it at once. Nothing books or fills
/// `vms` yet: the VM lease kind (`kbf-lease=vm`) is planned and refused today, and no
/// node's capacity carries VM slots.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Resources {
    /// CPU in thousandths of a core (1000 = one core).
    pub cpu_millis: u64,
    /// Memory in bytes.
    pub memory_bytes: u64,
    /// Whole GPUs.
    pub gpus: u64,
    /// VM slots.
    pub vms: u64,
}

impl Resources {
    /// A vector of `cpu_millis` thousandths of a core and `memory_bytes` bytes, and no
    /// GPU or VM slot.
    #[must_use]
    pub const fn new(cpu_millis: u64, memory_bytes: u64) -> Self {
        Self {
            cpu_millis,
            memory_bytes,
            gpus: 0,
            vms: 0,
        }
    }

    /// `self` with `gpus` whole GPUs.
    #[must_use]
    pub const fn with_gpus(self, gpus: u64) -> Self {
        Self { gpus, ..self }
    }

    /// `self` with `vms` VM slots.
    #[must_use]
    pub const fn with_vms(self, vms: u64) -> Self {
        Self { vms, ..self }
    }

    /// `self` with no memory: the vector admission compares an action's request on, since
    /// an action's memory is the limit it first runs under, not a booking.
    #[must_use]
    pub const fn without_memory(self) -> Self {
        Self {
            memory_bytes: 0,
            ..self
        }
    }

    /// Whether `request` fits in `self` on every axis.
    #[must_use]
    pub const fn fits(&self, request: &Self) -> bool {
        request.cpu_millis <= self.cpu_millis
            && request.memory_bytes <= self.memory_bytes
            && request.gpus <= self.gpus
            && request.vms <= self.vms
    }

    /// `self + other` on every axis, saturating.
    #[must_use]
    pub const fn saturating_add(self, other: Self) -> Self {
        Self {
            cpu_millis: self.cpu_millis.saturating_add(other.cpu_millis),
            memory_bytes: self.memory_bytes.saturating_add(other.memory_bytes),
            gpus: self.gpus.saturating_add(other.gpus),
            vms: self.vms.saturating_add(other.vms),
        }
    }

    /// `self - other` on every axis, saturating at zero.
    #[must_use]
    pub const fn saturating_sub(self, other: Self) -> Self {
        Self {
            cpu_millis: self.cpu_millis.saturating_sub(other.cpu_millis),
            memory_bytes: self.memory_bytes.saturating_sub(other.memory_bytes),
            gpus: self.gpus.saturating_sub(other.gpus),
            vms: self.vms.saturating_sub(other.vms),
        }
    }
}

/// What a worker does with a lease when it loses touch with the scheduler.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FencePolicy {
    /// Hermetic work: run to its timeout and keep the result. A duplicate run after
    /// re-dispatch costs only compute.
    RunOn,
    /// Networked work: stop once the newest acknowledged heartbeat is older than the
    /// self-fence time, so two copies never run at once.
    SelfFence,
}

/// Why an attempt failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Failure {
    /// The action ran past its timeout.
    Timeout,
    /// The farm failed the action (a kernel OOM, a lost container), not the action itself.
    /// The scheduler runs it again, preferring another node, while its farm reruns
    /// last; an operation that finishes with it has used them up.
    Infra,
    /// The action itself cannot run as written (an image named by tag, an output that
    /// is also an input): the client's error, answered INVALID_ARGUMENT. Running it
    /// elsewhere would fail the same way, so it is never retried and never cached.
    Invalid,
    /// The lease was killed because the action passed its own memory limit. Reported
    /// by a worker, the scheduler requeues it with its memory booking doubled, up to
    /// the largest node that fits it. An operation that finishes with it was killed
    /// at that largest node: the action needs more memory than any node offers.
    OutOfMemory,
    /// The node killed the lease for memory while the action was under its own limit
    /// (a node-wide out-of-memory kill, or the node's backstop): the busy node's
    /// fault, not the action's. The scheduler requeues it with the same booking while
    /// its farm reruns last; an operation that finishes with it has used them up.
    NodeMemoryPressure,
    /// The worker could not fetch an input blob: the CAS no longer holds it (the front
    /// checked the inputs before the operation was queued, so it was lost since). The
    /// callers are answered FAILED_PRECONDITION with a `MISSING` violation per blob, so
    /// a client that re-uploads (Bazel) executes again. Not a farm fault: it is never
    /// rerun, never cached, and counts against no farm rerun budget.
    MissingInput,
    /// The farm lost the run, and with it the result (`LostWhy` says how): not the
    /// action's failure. The scheduler runs it again, preferring another node, while
    /// its farm reruns last; an operation that finishes with it has used them up.
    Lost(LostWhy),
}

/// How the farm lost a lease's run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LostWhy {
    /// The daemon restarted while the action ran: the restarted daemon killed what
    /// was left of the run before it registered again, and its result is gone.
    DaemonRestarted,
    /// The node lost contact with the server and fenced (killed) the run: no
    /// heartbeat was acknowledged within the self-fence time.
    Fenced,
}

impl fmt::Display for LostWhy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::DaemonRestarted => "the daemon restarted while the action ran",
            Self::Fenced => "the node lost contact with the server and fenced the run",
        })
    }
}

/// Why a run ended as the farm's fault, counted against the operation's farm reruns
/// (`FARM_RERUNS` in `kbf-sched`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FarmFault {
    /// The node killed it for memory below the action's own limit.
    NodeMemoryPressure,
    /// The farm lost it.
    Lost(LostWhy),
    /// The farm could not run it ([`Failure::Infra`]): the daemon's driver failed
    /// before or around the action, not the action itself.
    Infra,
}

impl fmt::Display for FarmFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NodeMemoryPressure => f.write_str(
                "the node killed the action for memory while it was under its own limit \
                 (node memory pressure)",
            ),
            Self::Lost(why) => why.fmt(f),
            Self::Infra => f.write_str("the farm could not run the action (infra failure)"),
        }
    }
}

/// One run of an operation that ended as the farm's fault.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FarmRun {
    /// The node it ran on.
    pub worker: WorkerId,
    /// How it ended.
    pub fault: FarmFault,
}

/// How an attempt ended, as its worker reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Outcome {
    /// The action ran to completion (a failing test is still a completion); its
    /// `ActionResult` is stored under `action_result`.
    Completed {
        /// The digest of the uploaded `ActionResult`.
        action_result: Digest,
    },
    /// The attempt failed.
    Failed(Failure),
}

/// How a run was killed for memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryKill {
    /// The action passed its own memory limit. `cap` is the memory of the largest node
    /// that could run it when the kill was committed, in bytes.
    OwnLimit {
        /// The cap, in bytes.
        cap: u64,
    },
    /// The node killed it under memory pressure, below its own limit.
    NodePressure,
}

/// One run of an operation that was killed for memory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryRun {
    /// The node it ran on.
    pub worker: WorkerId,
    /// Its memory booking, in bytes.
    pub booked: u64,
    /// How it was killed.
    pub kill: MemoryKill,
}

/// A lease grant: `operation` runs on `worker` under `lease`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseGrant {
    /// The lease.
    pub lease: LeaseId,
    /// The operation it runs.
    pub operation: OperationId,
    /// Where.
    pub worker: WorkerId,
}

/// A result the scheduler accepted from the holder of `lease`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResultRecord {
    /// The lease the result came from.
    pub lease: LeaseId,
    /// The operation it answers.
    pub operation: OperationId,
    /// What happened.
    pub outcome: Outcome,
}

/// The scheduler gives up on a queued operation without running it: no live worker
/// could run it for the whole of the wait bound.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefusalRecord {
    /// The operation.
    pub operation: OperationId,
    /// Why no worker could run it, for its callers.
    pub reason: String,
}

/// An entry of the control log that the scheduler asks to commit.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlRecord {
    /// A lease grant. Its `Start` is sent only once this is committed.
    Lease(LeaseGrant),
    /// An accepted result. Its waiters are answered only once this is committed.
    Result(ResultRecord),
    /// A refusal. Its waiters are answered only once this is committed.
    Refusal(RefusalRecord),
}

/// Tells `worker` to run `operation` under a committed `lease`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartLease {
    /// The worker that runs it.
    pub worker: WorkerId,
    /// The committed lease.
    pub lease: LeaseId,
    /// The operation; the worker names it in its reports.
    pub operation: OperationId,
    /// The action to run.
    pub key: ActionKey,
    /// What the lease takes: a share of the worker, or all of it.
    pub kind: LeaseKind,
    /// What the scheduler booked for it on `worker`: the request for an action lease,
    /// the worker's whole capacity for a whole-machine one.
    pub resources: Resources,
    /// What the worker does if it loses touch.
    pub fence: FencePolicy,
    /// The iOS device booked for the lease, by id (the UDID), if the action asked for
    /// one (`ios.device=1`).
    pub device: Option<String>,
}

/// Answers every waiter of a finished operation with its committed result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    /// The operation.
    pub operation: OperationId,
    /// The lease whose result was accepted.
    pub lease: LeaseId,
    /// Every waiter attached to the operation, in attach order.
    pub waiters: Vec<WaiterId>,
    /// The result.
    pub outcome: Outcome,
    /// The operation's runs that were killed for memory, oldest first: what callers of
    /// an operation finished by a memory kill are told about.
    pub memory_runs: Vec<MemoryRun>,
    /// The operation's runs that ended as the farm's fault, oldest first: what callers
    /// of an operation whose farm reruns are used up are told about.
    pub farm_runs: Vec<FarmRun>,
}

/// Why a queued operation is waiting, when no live worker can run it now. Its callers
/// see the reason while it waits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Waiting {
    /// The operation.
    pub operation: OperationId,
    /// Why no live worker can run it, or `None` once one can again.
    pub reason: Option<String>,
}

/// Answers every waiter of an operation the scheduler refused to run, with the
/// committed reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// The operation.
    pub operation: OperationId,
    /// Every waiter attached to it, in attach order.
    pub waiters: Vec<WaiterId>,
    /// Why no worker could run it.
    pub reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches two farm faults that read the same to a caller (a restarted daemon
    /// taken for a fence, or either for a busy node's kill, or any for an infra
    /// failure), and a lost run that does not read as its cause.
    #[test]
    fn each_farm_fault_reads_differently() {
        let all = [
            FarmFault::NodeMemoryPressure,
            FarmFault::Lost(LostWhy::DaemonRestarted),
            FarmFault::Lost(LostWhy::Fenced),
            FarmFault::Infra,
        ];
        let text: Vec<String> = all.iter().map(ToString::to_string).collect();
        assert!(text[0].contains("node memory pressure"), "{}", text[0]);
        assert_eq!(text[1], "the daemon restarted while the action ran");
        assert_eq!(text[2], LostWhy::Fenced.to_string());
        assert!(text[2].contains("fenced"), "{}", text[2]);
        assert!(text[3].contains("infra"), "{}", text[3]);
        for (i, a) in text.iter().enumerate() {
            for b in &text[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    /// Catches: a fit test that checks one axis only, which would book memory a node
    /// does not have (or CPU it does not have).
    #[test]
    fn fits_needs_every_axis() {
        let free = Resources::new(4_000, 8 << 30);
        assert!(free.fits(&Resources::new(4_000, 8 << 30)));
        assert!(!free.fits(&Resources::new(4_001, 1)));
        assert!(!free.fits(&Resources::new(1, (8 << 30) + 1)));
    }

    /// Catches: `without_memory` that zeroes another axis, or leaves the memory.
    #[test]
    fn without_memory_zeroes_only_memory() {
        let r = Resources::new(2_000, 4 << 30).with_gpus(1).with_vms(2);
        assert_eq!(
            r.without_memory(),
            Resources::new(2_000, 0).with_gpus(1).with_vms(2)
        );
    }

    /// Catches: a fit test that ignores GPUs, which would place a GPU action on a node
    /// without one, or a second GPU action on a node whose only GPU is booked.
    #[test]
    fn fits_counts_whole_gpus() {
        let one_gpu = Resources::new(4_000, 8 << 30).with_gpus(1);
        let gpu_action = Resources::new(1_000, 1 << 30).with_gpus(1);
        assert!(one_gpu.fits(&gpu_action));
        assert!(!Resources::new(4_000, 8 << 30).fits(&gpu_action));
        assert!(!one_gpu.fits(&gpu_action.with_gpus(2)));
        assert!(
            one_gpu.fits(&Resources::new(1_000, 1 << 30)),
            "a CPU action"
        );
    }

    /// Catches: a fit test that ignores VM slots, which would place a VM lease on a node
    /// that runs no VM, or a third VM on a node with two slots both booked.
    #[test]
    fn fits_counts_vm_slots() {
        let room = Resources::new(12_000, 64 << 30);
        let vm_lease = Resources::new(6_000, 17 << 30).with_vms(1);
        assert!(!room.fits(&vm_lease), "a node with no VM slot");
        assert!(room.with_vms(1).fits(&vm_lease));
        let both_booked = room
            .with_vms(2)
            .saturating_sub(vm_lease.saturating_add(vm_lease));
        assert_eq!(both_booked.vms, 0);
        assert!(!both_booked.fits(&Resources::new(0, 0).with_vms(1)));
        assert!(
            room.fits(&Resources::new(1_000, 1 << 30)),
            "a bare-metal action"
        );
    }

    /// Catches: arithmetic that drops or wraps the VM axis, which would leave a slot
    /// booked after its lease ends or free a slot that is still booked.
    #[test]
    fn vm_arithmetic_saturates() {
        let two = Resources::default().with_vms(2);
        let one = Resources::default().with_vms(1);
        assert_eq!(one.saturating_add(one), two);
        assert_eq!(two.saturating_sub(one), one);
        assert_eq!(one.saturating_sub(two), Resources::default());
        assert_eq!(
            Resources::default()
                .with_vms(u64::MAX)
                .saturating_add(one)
                .vms,
            u64::MAX
        );
    }

    /// Catches: arithmetic that wraps, which would turn an overbooked node into one
    /// with nearly unlimited free room.
    #[test]
    fn arithmetic_saturates() {
        let a = Resources::new(1_000, 10);
        let b = Resources::new(3_000, 5);
        assert_eq!(a.saturating_sub(b), Resources::new(0, 5));
        assert_eq!(
            Resources::new(u64::MAX, 1).saturating_add(a),
            Resources::new(u64::MAX, 11)
        );
        let gpus = Resources::default().with_gpus(2);
        let one = Resources::default().with_gpus(1);
        assert_eq!(gpus.saturating_add(one), Resources::default().with_gpus(3));
        assert_eq!(one.saturating_sub(gpus), Resources::default());
        assert_eq!(gpus.saturating_sub(one), one);
        assert_eq!(
            Resources::default()
                .with_gpus(u64::MAX)
                .saturating_add(one)
                .gpus,
            u64::MAX
        );
    }
}
