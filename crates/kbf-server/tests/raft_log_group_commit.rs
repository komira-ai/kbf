//! Group commit in `RaftMetaLog`: proposals that arrive while a commit is in its disk
//! sync are written together and made durable by one sync, and each is answered only
//! after that sync. The raft directory is a `FaultFs` whose file syncs a test can hold
//! one at a time, or make panic once.

use std::future::Future;
use std::io;
use std::num::NonZeroU64;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::Poll;
use std::time::Duration;

use kbf_front::{MetaLog, MetaLogError};
use kbf_log::{FarmLogId, MachineVersion};
use kbf_meta::{
    Applied, BlobAnswer, Command, Epoch, Location, ObjectId, Retention, ServerId, StoreId, TierId,
};
use kbf_server::RaftMetaLog;
use kbf_store::{Fault, FaultFile, FaultFs, Fs, FsFile};
use kbf_types::{Digest, DigestFunction};
use tokio::runtime::Runtime;

/// How many proposals arrive while a sync is held.
const N: u8 = 8;

/// Long enough for a step the code under test takes at once.
const PROMPTLY: Duration = Duration::from_secs(10);

/// Holds every file sync while armed, until [`Gate::release`] lets one through.
#[derive(Debug)]
struct Gate {
    armed: AtomicBool,
    /// The next sync to start panics instead, before it reaches the gate.
    panic_next: AtomicBool,
    entered: Mutex<Sender<()>>,
    releases: Mutex<Receiver<()>>,
    release: Mutex<Sender<()>>,
}

impl Gate {
    fn new() -> (Arc<Self>, Receiver<()>) {
        let (entered, entered_rx) = mpsc::channel();
        let (release, releases) = mpsc::channel();
        let gate = Self {
            armed: AtomicBool::new(false),
            panic_next: AtomicBool::new(false),
            entered: Mutex::new(entered),
            releases: Mutex::new(releases),
            release: Mutex::new(release),
        };
        (Arc::new(gate), entered_rx)
    }

    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    /// Lets one held sync (or the next one) through.
    fn release(&self) {
        let _ = lock(&self.release).send(());
    }

    fn pass(&self) {
        if self.armed.load(Ordering::SeqCst) {
            let _ = lock(&self.entered).send(());
            let _ = lock(&self.releases).recv();
        }
    }
}

/// Disarms the gate and lets every sync through when the test ends, red or green, so
/// neither the log's drop (which waits for the batch it is writing) nor the runtime's
/// shutdown waits on a held commit forever. Declared after the log, so it is dropped
/// before it.
struct Disarm(Arc<Gate>);

impl Drop for Disarm {
    fn drop(&mut self) {
        self.0.armed.store(false, Ordering::SeqCst);
        for _ in 0..64 {
            self.0.release();
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A [`FaultFs`] whose file syncs pass through a [`Gate`].
#[derive(Debug)]
struct GateFs {
    inner: FaultFs,
    gate: Arc<Gate>,
}

#[derive(Debug)]
struct GateFile {
    inner: FaultFile,
    gate: Arc<Gate>,
}

impl Fs for GateFs {
    type File = GateFile;

    fn list(&self) -> io::Result<Vec<String>> {
        self.inner.list()
    }

    fn read(&self, name: &str) -> io::Result<Vec<u8>> {
        self.inner.read(name)
    }

    fn file_len(&self, name: &str) -> io::Result<u64> {
        self.inner.file_len(name)
    }

    fn read_at(&self, name: &str, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        self.inner.read_at(name, offset, len)
    }

    fn create(&self, name: &str) -> io::Result<GateFile> {
        let inner = self.inner.create(name)?;
        let gate = Arc::clone(&self.gate);
        Ok(GateFile { inner, gate })
    }

    fn create_positioned(&self, name: &str) -> io::Result<GateFile> {
        let inner = self.inner.create_positioned(name)?;
        let gate = Arc::clone(&self.gate);
        Ok(GateFile { inner, gate })
    }

    fn open_append(&self, name: &str) -> io::Result<GateFile> {
        let inner = self.inner.open_append(name)?;
        let gate = Arc::clone(&self.gate);
        Ok(GateFile { inner, gate })
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.inner.rename(from, to)
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        self.inner.remove(name)
    }

    fn sync_dir(&self) -> io::Result<()> {
        self.inner.sync_dir()
    }
}

impl FsFile for GateFile {
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.inner.append(bytes)
    }

    fn write_at(&self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        self.inner.write_at(offset, bytes)
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.inner.truncate(len)
    }

    fn sync(&self) -> io::Result<()> {
        assert!(
            !self.gate.panic_next.swap(false, Ordering::SeqCst),
            "a planted panic in a raft directory sync"
        );
        self.gate.pass();
        self.inner.sync()
    }
}

type Log = RaftMetaLog<GateFs>;
type Answer = Result<Applied, MetaLogError>;
type Pending = Pin<Box<dyn Future<Output = ()> + Send>>;

fn new_id() -> io::Result<FarmLogId> {
    Ok(FarmLogId::new(NonZeroU64::MIN))
}

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
}

/// A blob of its own for each `n`, in `epoch`.
fn blob(epoch: Epoch, n: u8) -> (Digest, Command) {
    let digest = Digest::new(DigestFunction::Sha256, [n; 32], 7);
    let location = Location {
        store: StoreId::new(0),
        object: ObjectId::new(epoch, u64::from(n) + 1),
        offset: 0,
    };
    (digest, Command::PutBlob { digest, location })
}

/// The log over `fs`, an epoch committed in it, and the gate armed: every sync from
/// here on waits for a release.
fn opened(runtime: &Runtime, fs: GateFs) -> (Arc<Log>, Epoch) {
    let gate = Arc::clone(&fs.gate);
    let log = runtime
        .block_on(RaftMetaLog::open(fs, Retention::default(), "kbf/", new_id))
        .expect("open");
    let epoch = match runtime.block_on(log.commit(Command::AllocEpoch)) {
        Ok(Applied::Epoch(e)) => e,
        other => panic!("AllocEpoch answered {other:?}"),
    };
    gate.arm();
    (Arc::new(log), epoch)
}

/// Starts a commit of `command` on its own thread; its answer arrives on the receiver.
fn commit_on_a_thread(runtime: &Runtime, log: &Arc<Log>, command: Command) -> Receiver<Answer> {
    let (tx, rx) = mpsc::channel();
    let (log, handle) = (Arc::clone(log), runtime.handle().clone());
    std::thread::spawn(move || {
        let _ = tx.send(handle.block_on(log.commit(command)));
    });
    rx
}

/// Proposes each of `commands` while a commit is held in its sync (see
/// [`poll_while_held`]); `(i, answer)` arrives on the receiver once commit `i` answers.
fn propose_while_held(
    runtime: &Runtime,
    log: &Arc<Log>,
    commands: Vec<Command>,
) -> Receiver<(usize, Answer)> {
    let (done, answers) = mpsc::channel();
    let pending = commands
        .into_iter()
        .enumerate()
        .map(|(i, command)| {
            let (log, done) = (Arc::clone(log), done.clone());
            Box::pin(async move {
                let _ = done.send((i, log.commit(command).await));
            }) as Pending
        })
        .collect();
    poll_while_held(runtime, pending);
    answers
}

/// Makes each of `pending`'s proposals while a commit is held in its sync: each is
/// polled once (which is when a proposal reaches the log) on a helper thread, then
/// spawned to finish.
///
/// # Panics
///
/// If the proposals are not all made within [`PROMPTLY`]: a log whose `commit` waits
/// for the commit in progress (one sync per proposal, under one lock) never gets past
/// the first.
fn poll_while_held(runtime: &Runtime, mut pending: Vec<Pending>) {
    let (made, proposed) = mpsc::channel();
    let handle = runtime.handle().clone();
    std::thread::spawn(move || {
        handle.block_on(std::future::poll_fn(|cx| {
            for commit in &mut pending {
                assert!(commit.as_mut().poll(cx).is_pending(), "answered while held");
            }
            Poll::Ready(())
        }));
        let _ = made.send(pending);
    });
    let pending = proposed
        .recv_timeout(PROMPTLY)
        .expect("every proposal was made while a sync was held");
    for commit in pending {
        drop(runtime.spawn(commit));
    }
}

/// For each `i` in `0..n`: a blob of its own when `i` is even, an `AllocEpoch` when
/// it is odd, so that callers in one batch are owed answers of two kinds, and every
/// `AllocEpoch` caller a different epoch. The digests are the blobs'.
fn mixed(epoch: Epoch, n: u8) -> (Vec<Digest>, Vec<Command>) {
    let mut digests = Vec::new();
    let commands = (0..n)
        .map(|i| {
            if i % 2 == 0 {
                let (digest, command) = blob(epoch, i + 1);
                digests.push(digest);
                command
            } else {
                Command::AllocEpoch
            }
        })
        .collect();
    (digests, commands)
}

/// Waits for `n` answers and asserts that each caller of [`mixed`] got the answer to
/// its own command: an even caller a written blob, an odd one an epoch later than
/// `after` and than every lower caller's. The batch's entries take consecutive
/// indices in the order their callers queued, so a caller answered with another
/// entry's outcome gets either the wrong kind or an epoch out of order. Returns the
/// last epoch.
fn assert_own_answers(answers: &Receiver<(usize, Answer)>, n: u8, after: Epoch) -> Epoch {
    let mut answered = vec![None; usize::from(n)];
    for _ in 0..n {
        let (i, answer) = answers
            .recv_timeout(PROMPTLY)
            .expect("every proposal answered after the sync that holds it");
        answered[i] = Some(answer);
    }
    let mut last = after;
    for (i, answer) in answered.into_iter().enumerate() {
        match answer {
            Some(Ok(Applied::Blob(Ok(_)))) if i % 2 == 0 => {}
            Some(Ok(Applied::Epoch(e))) if i % 2 == 1 && e > last => last = e,
            other => panic!("caller {i} (after epoch {last:?}) answered {other:?}"),
        }
    }
    last
}

fn seen(runtime: &Runtime, log: &Log, digest: Digest) -> BlobAnswer {
    runtime
        .block_on(log.query(move |m| m.blob(&digest)))
        .expect("a query")
}

/// Catches (red before group commit: the proposals never got past the first, which
/// waited under the log's lock for the held sync; mutants "one proposal per sync" and
/// "one persist per entry in a batch": the second sync then holds one proposal and the
/// rest wait for a third; mutant "answer before the sync": the proposals answer while
/// their sync is held; mutant "replies zipped in reverse": a caller gets another
/// entry's outcome, an epoch for a blob or a blob for an epoch): N proposals of two
/// kinds that arrive while a sync is held are written and
/// synced together, so the first sync and one more make every one durable, none
/// answers before that second sync, and none is visible to a query until it has. (The
/// store syncs inside each write, so applying before the sync is not a mutant this
/// layer can make.) Each caller gets the outcome of its own command. The first sync may
/// be a tick's rather than the first proposal's; the proposals then share the second
/// with it, which the test allows.
#[test]
fn proposals_made_during_a_sync_share_the_next_sync_and_answer_after_it() {
    let runtime = runtime();
    let (gate, entered) = Gate::new();
    let fs = GateFs {
        inner: FaultFs::new(),
        gate: Arc::clone(&gate),
    };
    let (log, epoch) = opened(&runtime, fs);
    let _disarm = Disarm(Arc::clone(&gate));
    let (first_digest, first) = blob(epoch, 0);
    let first = commit_on_a_thread(&runtime, &log, first);
    entered.recv_timeout(PROMPTLY).expect("the first sync");

    let (digests, commands) = mixed(epoch, N);
    let answers = propose_while_held(&runtime, &log, commands);
    gate.release();
    entered.recv_timeout(PROMPTLY).expect("the second sync");

    assert_eq!(
        answers.recv_timeout(Duration::from_millis(300)).err(),
        Some(RecvTimeoutError::Timeout),
        "a proposal answered before the sync that makes it durable"
    );
    for digest in &digests {
        assert_eq!(
            seen(&runtime, &log, *digest),
            BlobAnswer::Absent,
            "not yet durable"
        );
    }

    gate.release();
    let _ = assert_own_answers(&answers, N, epoch);
    let first = first.recv_timeout(PROMPTLY).expect("the first proposal");
    assert!(matches!(first, Ok(Applied::Blob(Ok(_)))), "{first:?}");
    for digest in digests.iter().chain([&first_digest]) {
        assert!(
            matches!(seen(&runtime, &log, *digest), BlobAnswer::Present { .. }),
            "a committed blob is not visible"
        );
    }
}

/// Catches (mutant "answer before the sync": the batch answers with outcomes it then
/// fails to make durable): the process dies in the sync of a batch. Every proposal of
/// the batch is answered, none with an outcome, and after a power cut none of them is
/// in the log.
#[test]
fn a_crash_in_a_batch_sync_answers_none_of_it_and_loses_all_of_it() {
    let runtime = runtime();
    let (gate, entered) = Gate::new();
    let inner = FaultFs::new();
    let fs = GateFs {
        inner: inner.clone(),
        gate: Arc::clone(&gate),
    };
    let (log, epoch) = opened(&runtime, fs);
    let _disarm = Disarm(Arc::clone(&gate));
    let first = commit_on_a_thread(&runtime, &log, blob(epoch, 0).1);
    entered.recv_timeout(PROMPTLY).expect("the first sync");

    let (digests, commands): (Vec<_>, Vec<_>) = (1..=N).map(|n| blob(epoch, n)).unzip();
    let answers = propose_while_held(&runtime, &log, commands);
    gate.release();
    entered.recv_timeout(PROMPTLY).expect("the second sync");
    // Held in the gate, the sync has not reached the directory: it is the next
    // operation, and the process dies there.
    inner.fail_at(inner.ops(), Fault::Crash);
    gate.release();

    for _ in 0..N {
        let (i, answer) = answers
            .recv_timeout(PROMPTLY)
            .expect("every proposal of the failed batch answered");
        assert!(answer.is_err(), "{i}: answered {answer:?} without a sync");
    }
    let _ = first.recv_timeout(PROMPTLY).expect("the first proposal");

    let cut = GateFs {
        inner: inner.crash(0),
        gate: Gate::new().0,
    };
    let reopened = runtime
        .block_on(RaftMetaLog::open(cut, Retention::default(), "kbf/", new_id))
        .expect("reopen after the power cut");
    for digest in digests {
        assert_eq!(
            seen(&runtime, &reopened, digest),
            BlobAnswer::Absent,
            "survived"
        );
    }
}

/// Catches (mutant "a committer stops guarding the queue once it has taken its batch":
/// the first half's arrival starts a second committer, which takes that half alone
/// while the first committer is still in its sync, so the second half waits for a
/// sync of its own, which the gate holds): proposals that arrive at two moments of one
/// held sync, with a pause between, still share the next sync, and each caller gets
/// its own command's outcome. The pause only gives such a second committer time to
/// take the first half; the log under test does not depend on it.
#[test]
fn proposals_made_at_two_moments_of_a_sync_share_the_next_sync() {
    let runtime = runtime();
    let (gate, entered) = Gate::new();
    let fs = GateFs {
        inner: FaultFs::new(),
        gate: Arc::clone(&gate),
    };
    let (log, epoch) = opened(&runtime, fs);
    let _disarm = Disarm(Arc::clone(&gate));
    let first = commit_on_a_thread(&runtime, &log, blob(epoch, 0).1);
    entered.recv_timeout(PROMPTLY).expect("the first sync");

    let (_, commands) = mixed(epoch, N);
    let mut commands = commands.into_iter();
    let early = propose_while_held(&runtime, &log, commands.by_ref().take(4).collect());
    std::thread::sleep(Duration::from_millis(300));
    let late = propose_while_held(&runtime, &log, commands.collect());
    gate.release();
    entered.recv_timeout(PROMPTLY).expect("the second sync");
    gate.release();

    // The late half's callers are numbered from 0 again; its first command is
    // `mixed`'s fifth, even, so the kinds stay in step. Its epochs follow the early
    // half's: it queued after them.
    let early_last = assert_own_answers(&early, 4, epoch);
    let _ = assert_own_answers(&late, N - 4, early_last);
    let first = first.recv_timeout(PROMPTLY).expect("the first proposal");
    assert!(matches!(first, Ok(Applied::Blob(Ok(_)))), "{first:?}");
}

/// Catches (mutant "a panic in a commit is not caught": the committer dies with its
/// queue marked as being committed, so no later command starts one and every later
/// commit, ticks included, waits forever): a sync that panics answers its batch
/// `Unavailable`, and a commit made after it is still answered (refused: the panic
/// stopped the host midway through its write).
#[test]
fn a_panic_in_a_batch_sync_answers_it_unavailable_and_later_commits_still_answer() {
    let runtime = runtime();
    let (gate, entered) = Gate::new();
    let fs = GateFs {
        inner: FaultFs::new(),
        gate: Arc::clone(&gate),
    };
    let (log, epoch) = opened(&runtime, fs);
    let _disarm = Disarm(Arc::clone(&gate));
    let first = commit_on_a_thread(&runtime, &log, blob(epoch, 0).1);
    entered.recv_timeout(PROMPTLY).expect("the first sync");
    // The held sync is past the panic check; the next batch's sync panics.
    gate.panic_next.store(true, Ordering::SeqCst);
    let (_, commands) = mixed(epoch, N);
    let answers = propose_while_held(&runtime, &log, commands);
    gate.release();

    for _ in 0..N {
        let (i, answer) = answers
            .recv_timeout(PROMPTLY)
            .expect("every proposal of the panicked batch answered");
        assert!(
            matches!(answer, Err(MetaLogError::Unavailable(_))),
            "{i}: answered {answer:?} from a batch whose sync panicked"
        );
    }
    assert!(
        !gate.panic_next.load(Ordering::SeqCst),
        "the planted panic did not fire"
    );
    let _ = first.recv_timeout(PROMPTLY).expect("the first proposal");

    gate.armed.store(false, Ordering::SeqCst);
    let later = commit_on_a_thread(&runtime, &log, blob(epoch, 9).1);
    let later = later
        .recv_timeout(PROMPTLY)
        .expect("a commit after the panic is answered");
    // The panic left the store's write half done, so the host has stopped: the
    // later commit is refused rather than written after a torn batch.
    assert!(
        matches!(later, Err(MetaLogError::Unavailable(_))),
        "{later:?}"
    );
}

/// Catches (mutant "a tick takes its own sync": the tick is proposed as a batch of
/// its own, so after the held sync the burst and the tick need a sync each, and the
/// second of them waits on a third sync, which the gate holds): a `Tick` proposed
/// while a burst of `PutBlobs` waits on a held sync joins the burst's batch. After
/// the held sync, one more sync (the gate lets through no third) answers every
/// `PutBlobs` caller with its own blob written and the tick with the farm time.
#[test]
fn a_tick_made_while_a_put_blobs_burst_waits_shares_its_next_sync() {
    let runtime = runtime();
    let (gate, entered) = Gate::new();
    let fs = GateFs {
        inner: FaultFs::new(),
        gate: Arc::clone(&gate),
    };
    let (log, epoch) = opened(&runtime, fs);
    let _disarm = Disarm(Arc::clone(&gate));
    let first = commit_on_a_thread(&runtime, &log, blob(epoch, 0).1);
    entered.recv_timeout(PROMPTLY).expect("the first sync");

    let (bursts, burst_answers) = mpsc::channel();
    let mut pending: Vec<Pending> = (1..=N)
        .map(|n| {
            let (digest, location) = match blob(epoch, n).1 {
                Command::PutBlob { digest, location } => (digest, location),
                other => unreachable!("{other:?}"),
            };
            let (log, bursts) = (Arc::clone(&log), bursts.clone());
            Box::pin(async move {
                let answer = log
                    .commit(Command::PutBlobs(vec![(digest, location)]))
                    .await;
                let _ = bursts.send((n, answer));
            }) as Pending
        })
        .collect();
    let (ticked, tick_answer) = mpsc::channel();
    let ticking = Arc::clone(&log);
    pending.push(Box::pin(async move {
        let _ = ticked.send(ticking.tick().await);
    }));
    poll_while_held(&runtime, pending);
    gate.release();
    entered.recv_timeout(PROMPTLY).expect("the second sync");
    gate.release();

    let tick = tick_answer
        .recv_timeout(PROMPTLY)
        .expect("the tick answered after the second sync");
    assert!(tick.is_ok(), "the tick answered {tick:?}");
    for _ in 1..=N {
        let (n, answer) = burst_answers
            .recv_timeout(PROMPTLY)
            .expect("every PutBlobs answered after the second sync");
        match answer {
            Ok(Applied::Blobs(Ok(written))) if written.len() == 1 => {}
            other => panic!("PutBlobs {n} answered {other:?}"),
        }
    }
    let first = first.recv_timeout(PROMPTLY).expect("the first proposal");
    assert!(matches!(first, Ok(Applied::Blob(Ok(_)))), "{first:?}");
}

/// Polls `future` once, outside any Tokio runtime, as kbf-sim-cell polls the log.
fn polled_once<F: Future>(future: F) -> Poll<F::Output> {
    let mut future = std::pin::pin!(future);
    future
        .as_mut()
        .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
}

/// Catches (the committer always spawned on Tokio's blocking pool: outside a runtime
/// that panics, "there is no reactor running", which is how kbf-sim-cell's server
/// tests failed): a caller that polls a commit by hand outside any runtime, with no
/// other commit in progress, commits on its own thread and is answered in that one
/// poll, and so is the next such caller.
#[test]
fn a_commit_polled_once_outside_a_runtime_commits_on_the_callers_thread() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let (gate, _) = Gate::new();
    let fs = GateFs {
        inner: FaultFs::new(),
        gate,
    };
    // The open spawns the tick task, so it needs a runtime's context; the runtime is
    // never driven, so the task never ticks.
    let opened = {
        let _context = runtime.enter();
        polled_once(RaftMetaLog::open(fs, Retention::default(), "kbf/", new_id))
    };
    let Poll::Ready(Ok(log)) = opened else {
        panic!("the open answered in one poll");
    };

    let epoch = match polled_once(log.commit(Command::AllocEpoch)) {
        Poll::Ready(Ok(Applied::Epoch(e))) => e,
        other => panic!("AllocEpoch polled once: {other:?}"),
    };
    let (digest, command) = blob(epoch, 1);
    let put = polled_once(log.commit(command));
    assert!(
        matches!(put, Poll::Ready(Ok(Applied::Blob(Ok(_))))),
        "{put:?}"
    );
    let seen = polled_once(log.query(move |m| m.blob(&digest)));
    assert!(
        matches!(seen, Poll::Ready(Ok(BlobAnswer::Present { .. }))),
        "{seen:?}"
    );
}

/// Commits `commands` as one batch: each is proposed while a commit of `first` is held
/// in its sync, and the gate lets that sync and one more through. Returns each
/// command's answer, in order.
fn one_batch(
    runtime: &Runtime,
    log: &Arc<Log>,
    (gate, entered): (&Gate, &Receiver<()>),
    first: Command,
    commands: Vec<Command>,
) -> Vec<Answer> {
    let n = commands.len();
    let first = commit_on_a_thread(runtime, log, first);
    entered.recv_timeout(PROMPTLY).expect("the first sync");
    let answers = propose_while_held(runtime, log, commands);
    gate.release();
    entered.recv_timeout(PROMPTLY).expect("the second sync");
    gate.release();
    let first = first.recv_timeout(PROMPTLY).expect("the held proposal");
    assert!(matches!(first, Ok(Applied::Blob(Ok(_)))), "{first:?}");
    let mut answered = vec![None; n];
    for _ in 0..n {
        let (i, answer) = answers
            .recv_timeout(PROMPTLY)
            .expect("every proposal answered after the second sync");
        answered[i] = Some(answer);
    }
    answered.into_iter().flatten().collect()
}

/// Lets every sync of `gate` through from now on, a held one included.
fn open_gate(gate: &Gate) {
    gate.armed.store(false, Ordering::SeqCst);
    gate.release();
}

fn add_tier() -> Command {
    Command::AddTier {
        server: ServerId::new(1),
        tier: TierId::new(1),
    }
}

fn tier_of_server_1(runtime: &Runtime, log: &Log) -> Option<TierId> {
    runtime
        .block_on(log.query(|m| m.tier(ServerId::new(1))))
        .expect("a query")
}

/// Catches (mutant "the batch is encoded in format 1", the group-commit side of the
/// merge with the machine-version raise: after the raise the tier's `AddTier` is
/// refused; mutant "a refused command takes the next command's answer" (`and` for
/// `and_then` where the batch's answers are handed out): before the raise the blob
/// after the refused `AddTier` is answered with the `AllocEpoch`'s epoch): a batch is
/// encoded in the format the log has committed when the batch commits. Before the raise
/// to machine version 2 a write tier command in a batch is refused and the rest of the
/// batch commits, each caller answered with its own outcome; after the raise the same
/// command commits in a batch with format-1 commands, and a restart replays both
/// batches (the version, the tier and the blobs come back). A raise commits alone
/// under the host's lock, so no batch holds a `MachineVersion` entry.
#[test]
fn a_batch_is_encoded_in_the_format_committed_before_it_and_replays() {
    let runtime = runtime();
    let (gate, entered) = Gate::new();
    let inner = FaultFs::new();
    let fs = GateFs {
        inner: inner.clone(),
        gate: Arc::clone(&gate),
    };
    let (log, epoch) = opened(&runtime, fs);
    let _disarm = Disarm(Arc::clone(&gate));
    let (digest_1, put_1) = blob(epoch, 1);
    let before = one_batch(
        &runtime,
        &log,
        (&gate, &entered),
        blob(epoch, 0).1,
        vec![add_tier(), put_1, Command::AllocEpoch],
    );
    match before.as_slice() {
        [
            Err(MetaLogError::Unavailable(_)),
            Ok(Applied::Blob(Ok(_))),
            Ok(Applied::Epoch(e)),
        ] if *e > epoch => {}
        other => panic!("before the raise the batch answered {other:?}"),
    }
    assert_eq!(tier_of_server_1(&runtime, &log), None);
    open_gate(&gate);
    drop(log);

    let inner = inner.restart();
    let (gate, entered) = Gate::new();
    let fs = GateFs {
        inner: inner.clone(),
        gate: Arc::clone(&gate),
    };
    let log = runtime
        .block_on(RaftMetaLog::open(fs, Retention::default(), "kbf/", new_id))
        .expect("reopen after the refused tier command");
    assert!(matches!(
        seen(&runtime, &log, digest_1),
        BlobAnswer::Present { .. }
    ));
    log.raise_machine_version(MachineVersion::V2)
        .expect("a raise");
    gate.arm();
    let log = Arc::new(log);
    let _disarm_raised = Disarm(Arc::clone(&gate));
    let (digest_2, put_2) = blob(epoch, 2);
    let (digest_3, put_3) = blob(epoch, 3);
    let after = one_batch(
        &runtime,
        &log,
        (&gate, &entered),
        blob(epoch, 4).1,
        vec![put_2, add_tier(), Command::AllocEpoch, put_3],
    );
    match after.as_slice() {
        [
            Ok(Applied::Blob(Ok(_))),
            Ok(Applied::Tier(Ok(()))),
            Ok(Applied::Epoch(_)),
            Ok(Applied::Blob(Ok(_))),
        ] => {}
        other => panic!("after the raise the batch answered {other:?}"),
    }
    open_gate(&gate);
    drop(log);

    let fs = GateFs {
        inner: inner.restart(),
        gate: Gate::new().0,
    };
    let log = runtime
        .block_on(RaftMetaLog::open(fs, Retention::default(), "kbf/", new_id))
        .expect("reopen after the raised batch");
    assert_eq!(log.machine_version(), MachineVersion::V2);
    assert_eq!(tier_of_server_1(&runtime, &log), Some(TierId::new(1)));
    for digest in [digest_1, digest_2, digest_3] {
        assert!(
            matches!(seen(&runtime, &log, digest), BlobAnswer::Present { .. }),
            "a committed blob did not replay"
        );
    }
}
