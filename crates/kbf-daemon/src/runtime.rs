//! Execution: the [`Runtime`] trait the lease manager runs work through (a daemon may
//! have several, one per lease kind: [`crate::Runtimes`]), and [`FakeRuntime`], which
//! runs nothing. [`crate::LocalRuntime`] (tests only) runs
//! actions as plain processes; the container driver (`kbf-driver-container`)
//! implements the trait for farm nodes.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use kbf_proto::reapi::{ActionResult, Digest};
use kbf_proto::worker::{InputCacheCounts, NodeLoad, ResourceUsage};
use kbf_types::{LeaseId, LeaseKind, LostWhy, Resources};
use tokio::sync::oneshot;

/// One lease's work, as the server's Start describes it.
#[derive(Clone, Debug, PartialEq)]
pub struct Work {
    pub lease_id: LeaseId,
    /// The lease kind (`action`, `whole_machine`).
    pub kind: String,
    pub action_digest: Digest,
    /// What the scheduler booked for the lease here; zero on an axis it did not book.
    pub resources: Resources,
}

/// Why work did not produce an action result.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// The work was stopped by [`Runtime::kill`].
    #[error("killed")]
    Killed,
    /// The runtime could not run the work (an infrastructure failure, not the action's).
    #[error("{0}")]
    Failed(String),
    /// The action asks for something no node may run (an image named by tag, an output
    /// path that leaves the working directory, a Command without arguments): the
    /// client's error, not the farm's.
    #[error("invalid action: {0}")]
    Invalid(String),
    /// A blob the action needs (`hash/size`) is not in the CAS: the client's to upload.
    #[error("blob {0} is not in the CAS")]
    MissingBlob(String),
    /// The action ran past its timeout and was stopped. `stdout` and `stderr` are what
    /// it wrote to each before then, stored in the CAS: REAPI's partial result, which
    /// the Result carries with DEADLINE_EXCEEDED. `None` for a log the driver left out
    /// (past the output limit, or not stored).
    #[error("timed out")]
    TimedOut {
        stdout: Option<Digest>,
        stderr: Option<Digest>,
    },
    /// The action's processes together used more memory than the lease allows, and
    /// the runtime (or the kernel, at the lease's own cap) killed them. Reported as
    /// RESOURCE_EXHAUSTED with `MEMORY_KILL_OWN_LIMIT`: the farm's to retry with more
    /// memory, not the client's to fix.
    #[error("out of memory: the action used {used} bytes, past the lease's limit of {limit}")]
    OutOfMemory { used: u64, limit: u64 },
    /// The kernel's OOM killer ended the action although it stayed within its lease's
    /// limit: the node ran short of memory (its cap on all actions, or the host). The
    /// string says which counters showed it. Reported as UNAVAILABLE with
    /// `MEMORY_KILL_NODE_PRESSURE`: the farm's to run again with the same booking.
    #[error("killed on a busy node: {0}")]
    BusyNode(String),
    /// The farm lost the run, and its result with it: the daemon restarted while the
    /// action ran, or the node fenced it. The string says what happened. Reported as
    /// UNAVAILABLE with `Result.lost`: the farm's to run again, preferring another node.
    #[error("{1}")]
    Lost(LostWhy, String),
}

/// How long a lease's whole output upload may take unless the daemon is told otherwise
/// (`--output-upload-deadline-secs`): 30 minutes.
pub const UPLOAD_DEADLINE: Duration = Duration::from_secs(30 * 60);

/// Runs `upload`, a lease's whole output upload (its output files, stdout and stderr),
/// for at most `deadline`. Past it, `upload` is dropped, which cancels its calls to the
/// store, and the lease fails with [`RuntimeError::Failed`] naming the upload, the
/// deadline and the time it ran: a store that stops answering ends the lease instead
/// of holding it with no error.
///
/// # Errors
/// What `upload` returns, or the failure past the deadline.
pub async fn upload_within<T>(
    deadline: Duration,
    upload: impl Future<Output = Result<T, RuntimeError>>,
) -> Result<T, RuntimeError> {
    let started = tokio::time::Instant::now();
    tokio::time::timeout(deadline, upload)
        .await
        .unwrap_or_else(|_| {
            Err(RuntimeError::Failed(format!(
                "the output upload did not finish within its deadline of {deadline:?} \
                 (--output-upload-deadline-secs): stopped after {:.1?}",
                started.elapsed()
            )))
        })
}

/// A runtime could not read the node's load. `kind` names what failed in a few fixed
/// words (a file name), so the daemon can log one kind once instead of on every heartbeat.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{kind}: {why}")]
pub struct NodeLoadError {
    pub kind: &'static str,
    pub why: String,
}

/// One reading of the node's actions cgroup and of each lease that runs in it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NodeReading {
    /// What a heartbeat carries of the node. The runtime leaves `lease_clocks` and
    /// `minutes` empty; the daemon's collector fills them from `leases` (and orders and
    /// cuts `covered`). (`NodeReading` is `PartialEq` and not `Eq` because the generated
    /// `NodeLoad` is not `Eq`.)
    pub load: NodeLoad,
    /// Each running lease of the runtime at the reading, in lease order.
    pub leases: Vec<LeaseReading>,
}

impl From<NodeLoad> for NodeReading {
    /// A reading that names no leases.
    fn from(load: NodeLoad) -> Self {
        Self {
            load,
            leases: Vec::new(),
        }
    }
}

/// One running lease at a [`NodeReading`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseReading {
    pub lease: LeaseId,
    /// When the lease's container started (after its inputs and image), on the clock of
    /// the reading's `load.mono_usec`.
    pub started_usec: u64,
    /// What its cgroup holds now; `None` when a file of it could not be read or parsed.
    /// The lease keeps its start then, and is never given a zero.
    pub usage: Option<LeaseUsage>,
}

/// What one lease's cgroup holds at a reading.
///
/// A lease's CPU peak is computed in two places, from different clock reads, so the two
/// can differ in the last digits and nothing asserts them equal. The container driver
/// takes the busiest window between a lease's own successive `cpu.stat` reads, each
/// stamped by a clock read right after that file ([`Runtime::take_peak_window_millicores`]).
/// A figure taken between whole readings uses the reading's `load.mono_usec`, one stamp for
/// every lease; the leases are read one after another, so a lease's own stamp is later than
/// the reading's by the time spent reading the files before it (tens of milliseconds with
/// many leases).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeaseUsage {
    /// `usage_usec` of the lease's `cpu.stat`: cumulative.
    pub cpu_usage_usec: u64,
    /// `memory.current` less `inactive_file` of `memory.stat`, floored at 0.
    pub working_set_bytes: u64,
}

/// Runs leases. The lease manager never names a driver: it asks the runtime whether it
/// serves a lease kind, and runs the lease through it.
pub trait Runtime: Send + Sync + 'static {
    /// The driver name the node report lists under `drivers`.
    fn driver(&self) -> &'static str;

    /// Whether this runtime runs leases of `kind`.
    fn serves(&self, kind: &str) -> bool;

    /// Runs one lease to completion. An action that runs and exits non-zero is `Ok`.
    fn run(&self, work: Work) -> impl Future<Output = Result<ActionResult, RuntimeError>> + Send;

    /// What the runtime measured of lease `lease_id`, whose [`Runtime::run`] ended in
    /// an error (timed out, killed, out of memory): the counters a result would have
    /// carried. Called once, after `run` returned the error; a second call, or a runtime
    /// that measures nothing (the default), returns `None`.
    fn failed_usage(&self, _lease_id: LeaseId) -> Option<ResourceUsage> {
        None
    }

    /// One reading of what the node's actions are doing and of each of this runtime's
    /// running leases, for the heartbeat. `Ok(None)` for a runtime that takes no readings
    /// (the default). The reading is made off the async workers.
    ///
    /// # Errors
    /// The reading could not be made.
    fn node_load(&self) -> impl Future<Output = Result<Option<NodeReading>, NodeLoadError>> + Send {
        async { Ok(None) }
    }

    /// What the runtime's input cache has done since the process started, for the
    /// heartbeat (`Heartbeat.input_cache`). `None` for a runtime with no input cache open
    /// (the default).
    fn input_cache(&self) -> Option<InputCacheCounts> {
        None
    }

    /// Told by each Welcome whether the farm places by the load the nodes measure
    /// (`Welcome.placement_load`, false when absent), on the connect of every stream, before
    /// the first heartbeat of it. A runtime that limits the CPU of its leases applies the
    /// mode here, off the async workers; one that does not (the default) ignores it. A failure
    /// is the runtime's to log: the stream goes on, and the next Welcome tells it again.
    fn welcomed_placement_load(&self, _on: bool) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// The most CPU any one window between two readings used by lease `lease_id`, in
    /// millicores, once, after its [`Runtime::run`] ended. `None` for a runtime that takes
    /// no readings (the default), and for a lease that ended before it had a full window.
    fn take_peak_window_millicores(&self, _lease_id: LeaseId) -> Option<u64> {
        None
    }

    /// Stops a running lease. Returns once its work has stopped; the lease's `run` then
    /// ends with [`RuntimeError::Killed`]. Killing a lease that is not running does
    /// nothing. The daemon calls it only after the lease's [`Runtime::run`] has been
    /// polled once, so `run` must register what `kill` stops before its first `.await`.
    fn kill(&self, lease_id: LeaseId) -> impl Future<Output = ()> + Send;
}

/// A runtime that runs nothing: each lease "succeeds" with exit code 0 and an empty
/// result after a fixed time, unless killed first. It records what it was asked to do,
/// for tests and for bringing up a daemon before a real driver exists.
#[derive(Debug)]
pub struct FakeRuntime {
    run_for: Duration,
    kind: LeaseKind,
    driver: &'static str,
    state: Mutex<FakeState>,
}

#[derive(Debug, Default)]
struct FakeState {
    started: Vec<LeaseId>,
    killed: Vec<LeaseId>,
    stop: BTreeMap<LeaseId, oneshot::Sender<()>>,
    readings: VecDeque<Result<Option<NodeReading>, NodeLoadError>>,
    hangs: bool,
}

impl FakeRuntime {
    /// Serves lease kind `action` as driver `fake`; each lease takes `run_for`.
    #[must_use]
    pub fn new(run_for: Duration) -> Self {
        Self {
            run_for,
            kind: LeaseKind::Action,
            driver: "fake",
            state: Mutex::new(FakeState::default()),
        }
    }

    /// This runtime serving lease kind `kind` alone, as driver `driver`: a daemon with
    /// several runtimes ([`crate::Runtimes`]) can be brought up, or tested, with fakes.
    #[must_use]
    pub fn serving(self, kind: LeaseKind, driver: &'static str) -> Self {
        Self {
            kind,
            driver,
            ..self
        }
    }

    /// This runtime answering its next [`Runtime::node_load`] with `reading`, after the
    /// readings already given (then `Ok(None)`, as a runtime that takes none does).
    #[must_use]
    pub fn with_node_load(self, reading: Result<Option<NodeReading>, NodeLoadError>) -> Self {
        self.lock().readings.push_back(reading);
        self
    }

    /// This runtime never answering [`Runtime::node_load`]: a read that hangs.
    #[must_use]
    pub fn with_hanging_node_load(self) -> Self {
        self.lock().hangs = true;
        self
    }

    /// Leases started, in order.
    #[must_use]
    pub fn started(&self) -> Vec<LeaseId> {
        self.lock().started.clone()
    }

    /// Leases killed while running, in order.
    #[must_use]
    pub fn killed(&self) -> Vec<LeaseId> {
        self.lock().killed.clone()
    }

    fn lock(&self) -> MutexGuard<'_, FakeState> {
        // The state stays consistent under a panic elsewhere: every update is one push
        // or one map operation.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Runtime for FakeRuntime {
    fn driver(&self) -> &'static str {
        self.driver
    }

    fn serves(&self, kind: &str) -> bool {
        kind == self.kind.name()
    }

    async fn run(&self, work: Work) -> Result<ActionResult, RuntimeError> {
        let (tx, rx) = oneshot::channel();
        {
            let mut state = self.lock();
            state.started.push(work.lease_id);
            state.stop.insert(work.lease_id, tx);
        }
        let outcome = tokio::select! {
            () = tokio::time::sleep(self.run_for) => Ok(ActionResult::default()),
            _ = rx => Err(RuntimeError::Killed),
        };
        self.lock().stop.remove(&work.lease_id);
        outcome
    }

    async fn kill(&self, lease_id: LeaseId) {
        let mut state = self.lock();
        if let Some(stop) = state.stop.remove(&lease_id) {
            state.killed.push(lease_id);
            // The receiver is gone only if the run already ended.
            let _ = stop.send(());
        }
    }

    async fn node_load(&self) -> Result<Option<NodeReading>, NodeLoadError> {
        if self.lock().hangs {
            std::future::pending::<()>().await;
        }
        self.lock().readings.pop_front().unwrap_or(Ok(None))
    }
}

#[cfg(test)]
mod tests {
    use futures::FutureExt as _;

    use super::*;

    /// Catches an upload that is not bounded (the lease waits on a store that never
    /// answers) and a failure that does not say it was the upload, its deadline and
    /// how long it ran. The clock is paused and the upload moves it 70 ms on its first
    /// poll, so the time it ran is exactly 70.0ms on any machine, and differs from the
    /// deadline: a message that printed the deadline twice fails.
    #[tokio::test(start_paused = true)]
    async fn an_upload_past_its_deadline_fails_saying_so() {
        let deadline = Duration::from_millis(50);
        // One expression: a line after an await that never resumes would read as
        // uncovered.
        let stuck = tokio::time::advance(Duration::from_millis(70))
            .then(|()| std::future::pending::<Result<(), RuntimeError>>());
        let ended = tokio::time::timeout(Duration::from_secs(10), upload_within(deadline, stuck))
            .await
            .expect("the upload ends at its deadline, not never");
        let why = format!("{:?}", ended.expect_err("the upload fails at its deadline"));
        assert_eq!(
            why,
            "Failed(\"the output upload did not finish within its deadline of 50ms \
             (--output-upload-deadline-secs): stopped after 70.0ms\")"
        );
    }

    /// Catches an upload that finishes in time being failed or changed.
    #[tokio::test]
    async fn an_upload_within_its_deadline_is_its_own_outcome() {
        let deadline = Duration::from_secs(10);
        assert_eq!(
            upload_within(deadline, async { Ok::<_, RuntimeError>(7) })
                .await
                .expect("ok"),
            7
        );
        let refused = upload_within(deadline, async {
            Err::<(), _>(RuntimeError::Invalid("bad".to_owned()))
        })
        .await
        .expect_err("the upload's own refusal");
        assert_eq!(format!("{refused:?}"), r#"Invalid("bad")"#);
    }
}
