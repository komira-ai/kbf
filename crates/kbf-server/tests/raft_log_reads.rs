//! `RaftMetaLog` queries while a commit is stalled in its disk sync: a read
//! (FindMissingBlobs, a blob read) must not wait for another caller's fsync, and must
//! not see the command that is not yet durable.

use std::io;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_front::MetaLog;
use kbf_log::FarmLogId;
use kbf_meta::{Applied, BlobAnswer, Command, Epoch, Location, ObjectId, Retention, StoreId};
use kbf_server::RaftMetaLog;
use kbf_store::{FaultFile, FaultFs, Fs, FsFile};
use kbf_types::{Digest, DigestFunction};

/// Holds the first file sync after [`Gate::arm`] until [`Gate::open`].
#[derive(Debug)]
struct Gate {
    armed: AtomicBool,
    entered: Mutex<Sender<()>>,
    release: Mutex<Receiver<()>>,
    open: Mutex<Sender<()>>,
}

impl Gate {
    fn new() -> (Arc<Self>, Receiver<()>) {
        let (entered, entered_rx) = mpsc::channel();
        let (open, release) = mpsc::channel();
        let gate = Self {
            armed: AtomicBool::new(false),
            entered: Mutex::new(entered),
            release: Mutex::new(release),
            open: Mutex::new(open),
        };
        (Arc::new(gate), entered_rx)
    }

    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    fn open(&self) {
        let _ = self
            .open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .send(());
    }

    fn pass(&self) {
        if self.armed.swap(false, Ordering::SeqCst) {
            let _ = self
                .entered
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .send(());
            let _ = self
                .release
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .recv();
        }
    }
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

    fn create(&self, name: &str) -> io::Result<GateFile> {
        let inner = self.inner.create(name)?;
        Ok(GateFile {
            inner,
            gate: Arc::clone(&self.gate),
        })
    }

    fn open_append(&self, name: &str) -> io::Result<GateFile> {
        let inner = self.inner.open_append(name)?;
        Ok(GateFile {
            inner,
            gate: Arc::clone(&self.gate),
        })
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

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.inner.truncate(len)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.gate.pass();
        self.inner.sync()
    }
}

fn blob_at(epoch: Epoch) -> (Digest, Command) {
    let digest = Digest::new(DigestFunction::Sha256, [7; 32], 7);
    let location = Location {
        store: StoreId::new(0),
        object: ObjectId::new(epoch, 1),
        offset: 0,
    };
    (digest, Command::PutBlob { digest, location })
}

/// Catches (red before this change; mutant "`query` takes the host lock"): a query
/// waiting for a commit's disk sync. That is how FindMissingBlobs queued behind each
/// second's `Tick` and every `PutBlobs` on a node whose fsyncs took seconds. A commit
/// is stalled in its sync; a query must still answer within 2 s, and must not see the
/// stalled command, which it must see once the commit has answered. (The store syncs
/// inside each persist, so moving the host's apply ahead of its own sync call is an
/// equivalent mutant here.)
#[test]
fn a_query_answers_while_a_commit_is_stalled_in_its_sync() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let (gate, entered) = Gate::new();
    let fs = GateFs {
        inner: FaultFs::new(),
        gate: Arc::clone(&gate),
    };
    let new_id = || Ok(FarmLogId::new(NonZeroU64::MIN));
    let log = Arc::new(
        runtime
            .block_on(RaftMetaLog::open(fs, Retention::default(), "kbf/", new_id))
            .expect("open"),
    );
    let epoch = match runtime.block_on(log.commit(Command::AllocEpoch)) {
        Ok(Applied::Epoch(e)) => e,
        other => panic!("AllocEpoch answered {other:?}"),
    };
    let (digest, put) = blob_at(epoch);

    gate.arm();
    let committing = std::thread::spawn({
        let (log, handle) = (Arc::clone(&log), runtime.handle().clone());
        move || handle.block_on(log.commit(put))
    });
    entered
        .recv_timeout(Duration::from_secs(10))
        .expect("a commit reached its sync");
    let (answered, answer) = mpsc::channel();
    std::thread::spawn({
        let (log, handle) = (Arc::clone(&log), runtime.handle().clone());
        move || {
            let seen = handle.block_on(log.query(|m| m.blob(&digest)));
            let _ = answered.send(seen);
        }
    });
    let during = answer.recv_timeout(Duration::from_secs(2));
    gate.open();
    let committed = committing.join().expect("join");

    let during = during.expect("the query answered while a commit was stalled in its sync");
    assert_eq!(
        during,
        Ok(BlobAnswer::Absent),
        "saw a command not yet durable"
    );
    assert!(
        matches!(committed, Ok(Applied::Blob(Ok(_)))),
        "{committed:?}"
    );
    let after = runtime.block_on(log.query(|m| m.blob(&digest)));
    assert!(matches!(after, Ok(BlobAnswer::Present { .. })), "{after:?}");
}
