//! Dropping a `RaftMetaLog` while a commit is still writing (kbf#651).
//!
//! A commit runs on tokio's blocking pool, apart from its caller, so the log can be
//! dropped while a commit it started is still writing to the raft directory. A reopen
//! of that directory in the same process then reads a log that is still growing:
//! `Store::open` reads the directory, the host reads it again, and an append landing
//! between the two leaves the host one entry past the store, so the open's first
//! write is refused ("entries do not continue the log").
//!
//! The raft directory here is a `FaultFs` whose next append, once armed, waits for the
//! test to let it through, so a commit can be held in the middle of its write.

use std::io;
use std::num::NonZeroU64;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use futures::FutureExt;
use kbf_front::MetaLog;
use kbf_log::FarmLogId;
use kbf_meta::{Command, Retention};
use kbf_server::RaftMetaLog;
use kbf_store::{FaultFile, FaultFs, Fs, FsFile};

/// How long the test gives a drop that must not return; one that does not wait
/// returns in microseconds.
const HELD: Duration = Duration::from_millis(500);

/// How long the test waits for something that must happen.
const PROMPT: Duration = Duration::from_secs(5);

/// Catches: a log whose drop returns while a commit it started is still writing (it
/// only stopped its tick task), so a reopen of the directory races that write; and a
/// commit queued behind it that is still written after the drop. The first commit is
/// held in its append while the log is dropped: the drop must wait for it, the held
/// commit must be in the reopened log, and the queued one must not.
#[tokio::test]
async fn dropping_the_log_waits_for_its_commit_in_flight_and_writes_nothing_after() {
    let gate = Arc::new(Gate::default());
    let fs = GatedFs {
        inner: FaultFs::new(),
        gate: Arc::clone(&gate),
    };
    let log = RaftMetaLog::open(fs.clone(), Retention::default(), "kbf/", new_log_id)
        .await
        .expect("open the log");
    let before = log.applied_index();

    // Neither commit is awaited: each is queued by its first poll, and the caller
    // stops waiting, as a caller dropped mid-commit does. Nothing here yields to the
    // runtime, so the log's tick task never runs.
    gate.arm();
    assert!(log.commit(Command::AllocEpoch).now_or_never().is_none());
    gate.wait_held();
    assert!(log.commit(Command::AllocEpoch).now_or_never().is_none());

    let dropper = std::thread::spawn(move || drop(log));
    let start = Instant::now();
    while !dropper.is_finished() && start.elapsed() < HELD {
        std::thread::sleep(Duration::from_millis(5));
    }
    let returned_early = dropper.is_finished();
    gate.open();
    dropper.join().expect("the drop does not panic");
    assert!(
        !returned_early,
        "the log's drop returned while its commit was still writing"
    );

    let log = RaftMetaLog::open(fs.inner.clone(), Retention::default(), "kbf/", new_log_id)
        .await
        .expect("reopen the log");
    // The held commit and the reopen's blank entry; not the queued commit.
    assert_eq!(log.applied_index(), before + 2);
}

fn new_log_id() -> io::Result<FarmLogId> {
    Ok(FarmLogId::new(NonZeroU64::MIN))
}

/// Holds the next append once armed, until opened.
#[derive(Debug, Default)]
struct Gate {
    state: Mutex<GateState>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct GateState {
    armed: bool,
    held: bool,
    open: bool,
}

impl Gate {
    fn lock(&self) -> MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn arm(&self) {
        self.lock().armed = true;
    }

    /// Called by an append: if armed, disarms, says it is held and waits until opened.
    fn pass(&self) {
        let mut state = self.lock();
        if !std::mem::take(&mut state.armed) {
            return;
        }
        state.held = true;
        self.changed.notify_all();
        while !state.open {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Waits until an append is held.
    fn wait_held(&self) {
        let state = self.lock();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, PROMPT, |s| !s.held)
            .unwrap_or_else(PoisonError::into_inner);
        assert!(state.held, "no append reached the gate within {PROMPT:?}");
    }

    fn open(&self) {
        self.lock().open = true;
        self.changed.notify_all();
    }
}

/// A [`FaultFs`] whose files' appends pass the [`Gate`].
#[derive(Clone, Debug)]
struct GatedFs {
    inner: FaultFs,
    gate: Arc<Gate>,
}

impl GatedFs {
    fn gated(&self, inner: FaultFile) -> GatedFile {
        GatedFile {
            inner,
            gate: Arc::clone(&self.gate),
        }
    }
}

impl Fs for GatedFs {
    type File = GatedFile;

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

    fn create(&self, name: &str) -> io::Result<GatedFile> {
        self.inner.create(name).map(|f| self.gated(f))
    }

    fn create_positioned(&self, name: &str) -> io::Result<GatedFile> {
        self.inner.create_positioned(name).map(|f| self.gated(f))
    }

    fn open_append(&self, name: &str) -> io::Result<GatedFile> {
        self.inner.open_append(name).map(|f| self.gated(f))
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

#[derive(Debug)]
struct GatedFile {
    inner: FaultFile,
    gate: Arc<Gate>,
}

impl FsFile for GatedFile {
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.gate.pass();
        self.inner.append(bytes)
    }

    fn write_at(&self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        self.inner.write_at(offset, bytes)
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.inner.truncate(len)
    }

    fn sync(&self) -> io::Result<()> {
        self.inner.sync()
    }
}
