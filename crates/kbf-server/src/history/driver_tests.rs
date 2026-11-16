//! The writer task's start, stop and drop. Each test names the defect it catches.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kbf_history::{HistoryRoot, IndexRoot};
use kbf_objstore::{Capabilities, KeyPrefix, MemoryStore};
use kbf_state::ManualClock;
use kbf_state::fake::{Fake, Flaw};
use tokio::sync::oneshot;
use tokio::time::Instant;

use super::{Abort, FINAL_STEP, Options, Running, STEP_EVERY, finish, over, spawn};

/// Sets its flag when it is dropped: a task's future is dropped when the task ends or is
/// aborted.
struct Dropped(Arc<AtomicBool>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Catches: a stop that waits for a last step that never ends (an object store that
/// never answers would hold the server's exit for ever), that gives up sooner than the
/// budget, or that leaves the task running after it gave up. Time is the paused clock's.
#[tokio::test(start_paused = true)]
async fn a_stop_gives_up_on_a_last_step_that_never_ends() {
    let (stop, stopped) = oneshot::channel::<()>();
    let gone = Arc::new(AtomicBool::new(false));
    let guard = Dropped(Arc::clone(&gone));
    let handle = tokio::spawn(async move {
        let _guard = guard;
        std::future::pending::<()>().await;
    });
    let running = Running {
        handle: Abort(handle),
        stop,
        workers: Vec::new(),
    };
    let begun = Instant::now();
    finish(Some(running)).await;
    assert_eq!(begun.elapsed(), FINAL_STEP);
    assert!(stopped.await.is_ok(), "the task was told to stop");
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(gone.load(Ordering::SeqCst), "the task was aborted");
}

/// Catches: a stop that waits out the whole budget for a writer that ends when told to
/// (a server's exit would take [`FINAL_STEP`] every time), and one that gives up on it as
/// if it had hung. Also a stop of nothing, which has nothing to wait for. Time is the
/// paused clock's.
#[tokio::test(start_paused = true)]
async fn a_stop_returns_as_soon_as_the_last_step_ends_and_nothing_stops_at_once() {
    let (stop, stopped) = oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        let _ = stopped.await;
    });
    let begun = Instant::now();
    finish(Some(Running {
        handle: Abort(handle),
        stop,
        workers: Vec::new(),
    }))
    .await;
    assert!(begun.elapsed() < FINAL_STEP, "{:?}", begun.elapsed());
    finish(None).await;
    assert_eq!(begun.elapsed(), Duration::ZERO);
}

/// Catches: a task that outlives its `Running` (a server that dropped its serving future
/// would leave a writer running against the stores), and a stop of nothing that waits.
#[tokio::test(start_paused = true)]
async fn dropping_a_running_writer_aborts_it_and_nothing_stops_at_once() {
    let (stop, _stopped) = oneshot::channel::<()>();
    let gone = Arc::new(AtomicBool::new(false));
    let guard = Dropped(Arc::clone(&gone));
    let handle = tokio::spawn(async move {
        let _guard = guard;
        std::future::pending::<()>().await;
    });
    drop(Running {
        handle: Abort(handle),
        stop,
        workers: Vec::new(),
    });
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(gone.load(Ordering::SeqCst), "aborted when dropped");
    assert!(spawn(None).is_none());
    let begun = Instant::now();
    finish(None).await;
    assert_eq!(begun.elapsed(), std::time::Duration::ZERO);
}

/// Catches: a writer id that is not the start's time (two starts would share one, and
/// their objects would collide on a key), and a step cadence that is not the one the index's transaction budget assumes.
#[test]
#[expect(
    clippy::clone_on_copy,
    reason = "the derived clone is the code the coverage ratchet counts, and is called here"
)]
fn a_start_is_named_by_its_time_and_steps_twice_a_second() {
    let a = Options::default();
    // Options are copied and shown as what they are.
    let copy = a.clone();
    assert_eq!(copy.writer, a.writer);
    assert!(format!("{copy:?}").contains("step_every"));
    assert_ne!(a.writer.0, 0, "microseconds since the epoch");
    assert_eq!(a.step_every, STEP_EVERY);
    assert_eq!(STEP_EVERY, std::time::Duration::from_millis(500));
}

/// Catches: a history that cannot be shown in a failure message (the tests that expect
/// it to be refused print it), and counters that are not those of the writer's own queue:
/// a new history has dropped nothing, holds nothing and says so in the Prometheus text.
#[test]
fn a_new_history_shows_as_itself_and_counts_nothing() {
    let store: Arc<dyn kbf_state::Store> = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let root = HistoryRoot::under(&KeyPrefix::new("kbf/").expect("a prefix")).expect("a root");
    let history = over(
        MemoryStore::new(Capabilities::default()),
        root,
        store,
        IndexRoot::default(),
        Options::default(),
    );
    assert_eq!(format!("{history:?}"), "History { .. }");
    let stats = history.stats();
    assert_eq!(stats.dropped("queue"), 0);
    assert_eq!(stats.dropped("no such stage"), 0);
    let mut text = String::new();
    stats.render(&mut text);
    assert!(text.contains("kbf_history_queue_bytes 0\n"), "{text}");
    assert!(
        text.contains("# TYPE kbf_history_dropped_total counter\n"),
        "{text}"
    );
    let _tap = history.tap();
}

/// The stop's flush over a real writer: the invocations a stop closes, and stores that
/// take nothing.
mod stop_flush {
    use kbf_front::Tap as _;
    use kbf_history::{
        EventQueue, HistoryIndex, InvKey, Limits, Minter, Offload, Offloaded, QueueLimits, Writer,
        WriterId,
    };
    use kbf_invocation::{
        ActionRow, Attempt, AttemptEnd, Ended, EndedKind, How, InvocationId, Outcome, RecordKey,
        WallTime,
    };
    use kbf_types::{Digest, DigestFunction, LeaseId, WorkerId};

    use super::*;
    use crate::history::driver::{Blocking, History, STALLED_STEPS, flush};
    use crate::history::reads::Shared;
    use crate::history::tap::HistoryTap;

    /// How many builds the first test records, named `b0`, `b1`, ...
    const BUILDS: usize = 12;

    fn at(us: u64) -> WallTime {
        WallTime::from_unix_micros(us)
    }

    fn key(id: &str) -> RecordKey {
        RecordKey::owner(InvocationId::new(id))
    }

    /// One action that ran once and passed.
    fn passed() -> ActionRow {
        ActionRow {
            action: Digest::new(DigestFunction::Sha256, [1; 32], 1),
            instance: "main".to_owned(),
            target: None,
            mnemonic: None,
            configuration: None,
            action_id: None,
            how: How::Ran,
            operation: Some("operations/x".to_owned()),
            attempts: vec![Attempt {
                lease: LeaseId::new(3, 9),
                worker: WorkerId::new("node-a"),
                queued: at(10_000),
                granted: at(20_000),
                started: Some(at(30_000)),
                ended: Some(at(40_000)),
                end: Some(AttemptEnd::Accepted),
                run: None,
                run_id: None,
            }],
            outcome: Some(Outcome::Ok { exit_code: 0 }),
            listing: None,
            outputs_truncated: false,
            command: None,
            shape: None,
            operation_id: None,
            evidence: None,
        }
    }

    /// The end of `id`'s one attempt.
    fn finished(id: &str) -> Ended {
        Ended {
            key: key(id),
            row: 0,
            attempt: Some(0),
            kind: EndedKind::Finished,
            at: at(50_000_000),
            correlated_invocations_id: None,
        }
    }

    fn state() -> Arc<dyn kbf_state::Store> {
        Arc::new(Fake::with_flaw(
            Arc::new(ManualClock::default()),
            Flaw::None,
        ))
    }

    fn root() -> HistoryRoot {
        HistoryRoot::under(&KeyPrefix::new("kbf/").expect("a prefix")).expect("a root")
    }

    /// Catches: a stop that takes one last step (one index transaction) and exits, which
    /// leaves every build past the first batch of rows unwritten and its header open at
    /// each stop of a rolling update; and a flush that stops after a fixed number of
    /// steps while each step still writes, or that takes a step whose stored-header
    /// reads add as many rows as its transaction wrote for no headway. Rows are four to
    /// a transaction and headers are read two a step here, and no step before the stop
    /// cuts a chunk, so only the flush writes. Each build the frontend was still serving
    /// is then read back closed, complete, with its one run.
    #[tokio::test]
    async fn a_stop_writes_every_build_it_closes_however_many_transactions_that_takes() {
        let objects = Arc::new(MemoryStore::new(Capabilities {
            conditional_put: true,
            ..Capabilities::default()
        }));
        let mut options = Options {
            step_every: Duration::from_secs(3600),
            ..Options::default()
        };
        options.limits.txn_rows = 4;
        options.limits.header_reads_per_step = 2;
        let history = over(
            Shared(Arc::clone(&objects)),
            root(),
            state(),
            IndexRoot::default(),
            options,
        );
        let tap = history.tap();
        for i in 0..BUILDS {
            let id = format!("b{i}");
            assert!(
                tap.ended(&finished(&id), &passed()).is_some(),
                "{id} queued"
            );
        }
        let History {
            task, stop, reads, ..
        } = history;
        let _ = stop.send(());
        task.await;
        let all = |_: &str| true;
        for i in 0..BUILDS {
            let id = format!("b{i}");
            let inv = InvKey::from_record_id(&key(&id).to_string());
            let view = reads
                .invocation(&inv, &all)
                .await
                .expect("the index answers")
                .unwrap_or_else(|| panic!("{id} was written"));
            assert!(view.complete, "{id} is closed and whole: {view:?}");
            assert_eq!(view.header.runs, 1, "{id}");
        }
    }

    /// An offload that never runs its job: every index call fails, as a store that is
    /// down does.
    struct Down;

    impl Offload for Down {
        fn offload(&self, job: Box<dyn FnOnce() + Send>) -> Offloaded {
            drop(job);
            Box::pin(async {})
        }
    }

    /// Catches: a stop that steps for as long as anything is owed when the index takes
    /// nothing (it would spin until the stop's budget aborts it, for every stop while the
    /// state store is down), and one that gives up before [`STALLED_STEPS`] failed steps.
    /// Here the steps alternate: one sends the rows and the transaction fails, the next
    /// restores them from the written chunks and sends nothing, so the failed steps are
    /// every other one after the first. The rows it could not write are dropped and
    /// counted, and the builds stay held. A writer with nothing to write takes one step.
    #[tokio::test]
    async fn a_stop_whose_index_takes_nothing_gives_up_after_the_stalled_steps() {
        let queue = Arc::new(EventQueue::new(QueueLimits::default()));
        let minter = Arc::new(Minter::for_writer(7));
        let index = HistoryIndex::new(state(), IndexRoot::default(), Arc::new(Down));
        let mut writer = Writer::new(
            MemoryStore::new(Capabilities::default()),
            root(),
            index,
            WriterId(7),
            Arc::clone(&minter),
            Arc::clone(&queue),
            Limits::default(),
        );
        let now = || 60_000_000;
        assert_eq!(flush(&mut writer, &now).await, 1, "nothing to write");
        let tap = HistoryTap::new(Arc::clone(&queue), minter);
        for i in 0..3 {
            tap.ended(&finished(&format!("b{i}")), &passed());
        }
        tap.close_all();
        let steps = flush(&mut writer, &now).await;
        assert!(
            (1 + STALLED_STEPS..=1 + 2 * STALLED_STEPS).contains(&steps),
            "{steps} steps"
        );
        assert_eq!(writer.open_invocations(), 3, "the builds are still held");
        assert!(writer.drops().index > 0, "{:?}", writer.drops());
    }

    /// Catches: a stop that steps for as long as a build is held when nothing fails and
    /// nothing changes (a build whose close the full queue dropped stays open, and the
    /// stop would spin until its budget aborts it), and one that gives up while the steps
    /// still write. The index answers here; the build's run is written up to what an
    /// open build can be, and the flush then ends with it still held and nothing failed.
    #[tokio::test]
    async fn a_stop_with_a_build_that_never_closes_ends_once_nothing_changes() {
        let queue = Arc::new(EventQueue::new(QueueLimits::default()));
        let minter = Arc::new(Minter::for_writer(9));
        let index = HistoryIndex::new(state(), IndexRoot::default(), Arc::new(Blocking));
        let mut writer = Writer::new(
            MemoryStore::new(Capabilities::default()),
            root(),
            index,
            WriterId(9),
            Arc::clone(&minter),
            Arc::clone(&queue),
            Limits::default(),
        );
        let tap = HistoryTap::new(Arc::clone(&queue), minter);
        tap.ended(&finished("open"), &passed());
        let steps = flush(&mut writer, &|| 60_000_000).await;
        assert!(steps > STALLED_STEPS, "{steps} steps");
        assert_eq!(writer.open_invocations(), 1, "the build is still held");
        assert_eq!(writer.pending_rows(), 0, "nothing is left to send");
        assert_eq!(writer.drops().put + writer.drops().index, 0);
    }
}
