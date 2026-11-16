//! The history writer's task: a step every [`STEP_EVERY`], and, when the server stops,
//! every open invocation closed and steps until nothing is left to write ([`flush`]),
//! all within [`FINAL_STEP`].

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::future::BoxFuture;
use kbf_front::Tap;
use kbf_history::{
    EventQueue, FlogObject, FlogParts, FlogRef, FlogStore, HistoryIndex, HistoryRoot, IndexRoot,
    Limits, Minter, Offload, Offloaded, QueueLimits, Reader, RunId, Writer, WriterId,
};
use kbf_objstore::{ObjectStore, ObjectStoreError};
use kbf_state::Store;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::reads::{HistoryReads, Shared};
use super::stats::Stats;
use super::tails::{BlobReads, FlogSink, Parts, TailOptions, Workers, pool};
use super::tap::HistoryTap;
use crate::http_blobs::ReadBudget;

/// How often the writer takes its queue: twice a second, which keeps the index
/// transactions at the two a second the writer's own bound allows.
pub const STEP_EVERY: Duration = Duration::from_millis(500);

/// How long the stop's flush, at a stop, may take before the task is given up.
const FINAL_STEP: Duration = Duration::from_secs(5);

/// Stalled steps (a write failed, or nothing changed) the stop's flush takes with no
/// headway between them before it gives up: the stores are not taking the writes, and
/// what is left is lost as a running writer's would be when its stores fail.
const STALLED_STEPS: usize = 3;

/// How long a stop waits for the failed runs still in the tail pool before the flush.
/// Inside [`FINAL_STEP`]: a pool that does not drain leaves its runs to be lost, not the
/// stop to hang.
const TAILS_STOP: Duration = Duration::from_secs(2);

/// What a history writer is made with besides its stores.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// The id the objects it writes carry: new for each start.
    pub writer: WriterId,
    /// The writer's bounds.
    pub limits: Limits,
    /// The queue's bounds.
    pub queue: QueueLimits,
    /// The time between steps.
    pub step_every: Duration,
    /// The tail workers' pool and waits, once the server gives history a blob source
    /// ([`History::with_blobs`]).
    pub tails: TailOptions,
}

impl Default for Options {
    fn default() -> Self {
        let since = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        Self {
            // Microseconds since the epoch: no two starts of one process, or of two
            // processes a microsecond apart, share one.
            writer: WriterId(u64::try_from(since.as_micros()).unwrap_or(u64::MAX)),
            limits: Limits::default(),
            queue: QueueLimits::default(),
            step_every: STEP_EVERY,
            tails: TailOptions::default(),
        }
    }
}

/// A history writer that has been made and not started: its tap for the recorder, its
/// counters for `/metrics`, and the task that writes.
pub struct History {
    tap: Arc<HistoryTap>,
    queue: Arc<EventQueue>,
    stats: Arc<Stats>,
    reads: Arc<HistoryReads>,
    flog: Arc<dyn FlogSink>,
    tails: TailOptions,
    /// The tail workers, made with the blob source and started with the writer.
    workers: Option<Workers>,
    task: Pin<Box<dyn Future<Output = ()> + Send>>,
    stop: oneshot::Sender<()>,
}

impl History {
    /// The tap the recorder's task is given ([`kbf_front::Recording::with_tap`]).
    #[must_use]
    pub fn tap(&self) -> Arc<dyn Tap> {
        let tap: Arc<dyn Tap> = self.tap.clone();
        tap
    }

    /// This history, reading the logs of failed runs from `blobs` under `budget`: each
    /// failed run is offered to the writer with its tails and command rendering, after its
    /// stdout, stderr and ActionResult are copied into an object of their own
    /// ([`super::tails`]). Without a blob source a failed run is offered with none.
    ///
    /// A source given before stays: the server gives its own only to a history that was
    /// not given one.
    #[must_use]
    pub fn with_blobs(mut self, blobs: Arc<dyn BlobReads>, budget: ReadBudget) -> Self {
        if self.tap.has_tails() {
            return self;
        }
        let parts = Parts {
            queue: Arc::clone(&self.queue),
            blobs,
            budget,
            flog: Arc::clone(&self.flog),
            stats: Arc::clone(&self.stats),
            open: self.tap.open_set(),
        };
        let (tails, workers) = pool(parts, self.tails);
        self.tap.install_tails(tails);
        self.workers = Some(workers);
        self
    }

    /// What the build pages read history with.
    #[must_use]
    pub fn reads(&self) -> Arc<HistoryReads> {
        Arc::clone(&self.reads)
    }

    /// What history counts.
    #[must_use]
    pub fn stats(&self) -> Arc<Stats> {
        Arc::clone(&self.stats)
    }
}

impl std::fmt::Debug for History {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("History").finish_non_exhaustive()
    }
}

/// Index calls on the runtime's blocking pool: the state store's calls block.
struct Blocking;

impl Offload for Blocking {
    fn offload(&self, job: Box<dyn FnOnce() + Send>) -> Offloaded {
        let handle = tokio::task::spawn_blocking(job);
        Box::pin(async move {
            // A job that never ran (the pool is shutting down) is answered by the index
            // as a failed call.
            let _ = handle.await;
        })
    }
}

impl<O: ObjectStore> FlogSink for FlogStore<Shared<O>> {
    fn encode(&self, run: RunId, parts: &FlogParts<'_>) -> FlogObject {
        FlogStore::encode(self, run, parts)
    }

    fn put(&self, object: FlogObject) -> BoxFuture<'_, Result<FlogRef, ObjectStoreError>> {
        Box::pin(FlogStore::put(self, object))
    }
}

/// A history writer over `objects` (under `root`) and the rows of `index_root` in
/// `state`, not yet started: [`spawn`] starts it. `objects` is wrapped in the writer's
/// own `Scoped` store, which refuses every key outside `<root>history/`.
pub fn over<O: ObjectStore>(
    objects: O,
    root: HistoryRoot,
    state: Arc<dyn Store>,
    index_root: IndexRoot,
    options: Options,
) -> History {
    let queue = Arc::new(EventQueue::new(options.queue));
    let minter = Arc::new(Minter::for_writer(options.writer.0));
    let index = HistoryIndex::new(state, index_root, Arc::new(Blocking));
    let objects = Shared(Arc::new(objects));
    let reader = Reader::new(objects.clone(), root.clone(), index.clone());
    let flog = Arc::new(FlogStore::new(
        objects.clone(),
        root.clone(),
        options.writer,
    ));
    let writer = Writer::new(
        objects,
        root,
        index,
        options.writer,
        Arc::clone(&minter),
        Arc::clone(&queue),
        options.limits,
    );
    let today = writer.today();
    let tap = Arc::new(HistoryTap::new(Arc::clone(&queue), minter));
    let reads = Arc::new(HistoryReads::new(reader, tap.runs(), today));
    let stats = Arc::new(Stats::new(Arc::clone(&queue)));
    let (stop, stopped) = oneshot::channel();
    let task = Box::pin(run(
        writer,
        Arc::clone(&tap),
        Arc::clone(&stats),
        options.step_every,
        stopped,
    ));
    History {
        tap,
        queue,
        stats,
        reads,
        flog,
        tails: options.tails,
        workers: None,
        task,
        stop,
    }
}

/// The wall clock in milliseconds, as the writer is given it.
fn now_ms() -> u64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
}

async fn run<O: ObjectStore>(
    mut writer: Writer<O>,
    tap: Arc<HistoryTap>,
    stats: Arc<Stats>,
    every: Duration,
    mut stopped: oneshot::Receiver<()>,
) {
    // The first step is one interval after the start: a step with nothing queued yet
    // would only reserve a stream block the first record will ask for anyway.
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let publish = |writer: &Writer<O>| {
        stats.publish(
            writer.drops(),
            writer.pending_rows(),
            writer.open_invocations(),
        );
    };
    loop {
        tokio::select! {
            biased;
            _ = &mut stopped => break,
            _ = tick.tick() => {}
        }
        writer.step(now_ms()).await;
        publish(&writer);
    }
    // Failed runs still in the tail pool are given a moment to reach the queue, then
    // every invocation offered a record is closed, so the flush writes its chunk.
    let _ = tokio::time::timeout(TAILS_STOP, tap.quiesce()).await;
    tap.close_all();
    flush(&mut writer, &now_ms).await;
    publish(&writer);
}

/// What the stop's flush watches of a writer: what it still has to write (index rows
/// waiting, objects whose rows wait to be restored, chunks cut and not written, catalog
/// and status rows owed, and the invocations it holds, each let go only once it is
/// closed and its chunk and header are written), and how many of its writes failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Left {
    owed: [usize; 5],
    failed: u64,
}

impl Left {
    fn of<O: ObjectStore>(writer: &Writer<O>) -> Left {
        let drops = writer.drops();
        Left {
            owed: [
                writer.pending_rows(),
                writer.kept_objects(),
                writer.unwritten_chunks(),
                writer.index_owed(),
                writer.open_invocations(),
            ],
            failed: drops.put + drops.index,
        }
    }

    fn is_empty(&self) -> bool {
        self.owed.iter().all(|n| *n == 0)
    }
}

/// The stop's flush: steps `writer` until it owes nothing, or until [`STALLED_STEPS`]
/// steps have stalled with no headway between them. A step makes headway when any of
/// the counts of [`Left`] falls below the least it has been; it stalls when a write of
/// it failed or it changed nothing. A step that only moves work along (a stored header
/// read becomes a row to write) does neither. One step sends at most
/// [`kbf_history::Limits::txns_per_step`] index transactions, so a single last step
/// would leave the rows of every invocation past the first batch unwritten, and their
/// headers open, at every stop of a rolling update. Each step yields, so the stop's
/// bound ([`FINAL_STEP`]) can always end it. Returns the steps taken.
async fn flush<O: ObjectStore>(writer: &mut Writer<O>, now_ms: &(dyn Fn() -> u64 + Sync)) -> usize {
    let (mut before, mut least) = (Left::of(writer), [usize::MAX; 5]);
    let (mut stalled, mut steps) = (0, 0);
    loop {
        writer.step(now_ms()).await;
        steps += 1;
        let after = Left::of(writer);
        if after.is_empty() {
            return steps;
        }
        let headway = after.owed.iter().zip(&least).any(|(now, low)| now < low);
        for (low, now) in least.iter_mut().zip(after.owed) {
            *low = (*low).min(now);
        }
        if headway {
            stalled = 0;
        } else if after.failed > before.failed || after == before {
            stalled += 1;
            if stalled == STALLED_STEPS {
                return steps;
            }
        }
        before = after;
        tokio::task::yield_now().await;
    }
}

/// A spawned task, aborted when this is dropped.
struct Abort(JoinHandle<()>);

impl Drop for Abort {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A started history writer: dropped, its task is aborted.
pub(crate) struct Running {
    handle: Abort,
    stop: oneshot::Sender<()>,
    /// The tail workers: aborted with the writer.
    workers: Vec<Abort>,
}

/// Starts `history`'s task on the runtime, if there is a history.
pub(crate) fn spawn(history: Option<History>) -> Option<Running> {
    let History {
        task,
        stop,
        workers,
        ..
    } = history?;
    Some(Running {
        handle: Abort(tokio::spawn(task)),
        stop,
        workers: workers
            .map(Workers::spawn)
            .unwrap_or_default()
            .into_iter()
            .map(Abort)
            .collect(),
    })
}

/// Stops `running` as a server's stop does: its writer closes every open invocation and
/// flushes ([`flush`]), for at most [`FINAL_STEP`].
pub(crate) async fn finish(running: Option<Running>) {
    let Some(Running {
        mut handle,
        stop,
        workers: _workers,
    }) = running
    else {
        return;
    };
    let _ = stop.send(());
    if tokio::time::timeout(FINAL_STEP, &mut handle.0)
        .await
        .is_err()
    {
        // Dropping `handle` aborts the task.
        tracing::warn!("history: the last step took over {FINAL_STEP:?}; it was dropped");
    }
}

#[cfg(test)]
#[path = "driver_tests.rs"]
mod tests;
