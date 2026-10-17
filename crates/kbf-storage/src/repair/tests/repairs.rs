//! [`Repairs`] over a disk that panics, and the ledger that tells no one.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kbf_store::{FaultFile, FaultFs, Fs, FsFile};

use super::{digest, record};
use crate::StorageError;
use crate::repair::{NoLedger, Options, RepairLedger, Repairs};
use crate::service::tests::fake::Mem;

/// A [`FaultFs`] whose files panic when they are appended to, once armed: the failure of a
/// blocking call that a journal error does not describe.
struct PanicFs {
    inner: FaultFs,
    armed: Arc<AtomicBool>,
}

struct PanicFile {
    inner: FaultFile,
    armed: Arc<AtomicBool>,
}

impl Fs for PanicFs {
    type File = PanicFile;

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

    fn create(&self, name: &str) -> io::Result<PanicFile> {
        Ok(PanicFile {
            inner: self.inner.create(name)?,
            armed: Arc::clone(&self.armed),
        })
    }

    fn create_positioned(&self, name: &str) -> io::Result<PanicFile> {
        Ok(PanicFile {
            inner: self.inner.create_positioned(name)?,
            armed: Arc::clone(&self.armed),
        })
    }

    fn open_append(&self, name: &str) -> io::Result<PanicFile> {
        Ok(PanicFile {
            inner: self.inner.open_append(name)?,
            armed: Arc::clone(&self.armed),
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

impl FsFile for PanicFile {
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        assert!(!self.armed.load(Ordering::SeqCst), "the disk panics");
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

/// Catches a panic in the journal's blocking call taken for success or for a journal error
/// (it would then be told as "stopped" and so as a disk fault): the call fails INTERNAL, the
/// debt is not recorded, and the debts already owed can still be read.
#[tokio::test]
async fn a_panic_in_the_journal_fails_the_call_and_loses_no_debt() {
    let armed = Arc::new(AtomicBool::new(false));
    let fs = PanicFs {
        inner: FaultFs::new(),
        armed: Arc::clone(&armed),
    };
    let repairs = Repairs::open(fs, Options::default(), Arc::new(NoLedger)).expect("open");
    drop(repairs.owe(record(1)).await.expect("owed"));
    armed.store(true, Ordering::SeqCst);
    let answer = repairs.owe(record(2)).await;
    assert!(
        matches!(answer, Err(StorageError::Internal(_))),
        "{answer:?}"
    );
    assert_eq!(repairs.open_records(), [record(1)]);
    // The same for settling a debt: the call fails, and the debt is still owed.
    let answer = repairs.settle(&digest(1)).await;
    assert!(
        matches!(answer, Err(StorageError::Internal(_))),
        "{answer:?}"
    );
    assert_eq!(repairs.open_records(), [record(1)]);
    let answer = repairs
        .settle_evicted(&digest(1), repairs.begun(), &Mem::default())
        .await;
    assert!(
        matches!(answer, Err(StorageError::Internal(_))),
        "{answer:?}"
    );
    assert_eq!(repairs.open_records(), [record(1)]);
}

/// Catches a ledger that tells no one being asked for something: both of its reports return
/// at once.
#[tokio::test]
async fn the_ledger_that_tells_no_one_returns_at_once() {
    NoLedger.owed(&record(1)).await;
    NoLedger.settled(&digest(1)).await;
}

/// Catches a debt dropped for an absent blob while a write of it is committing, or one that
/// began owing after the worker looked: `settle_evicted` leaves the debt while a write of the
/// digest is in flight (however many: two writes of the same blob, one finished), and when
/// any write began owing since `begun` was read; with neither, it settles and the journal
/// forgets it. Mutants: the in-flight test removed; the `begun` comparison removed; the
/// count of a digest's writes dropped to zero by the first.
#[tokio::test]
async fn an_evicted_debt_is_dropped_only_when_no_write_is_in_its_window() {
    let store = Mem::default();
    let repairs =
        Repairs::open(FaultFs::new(), Options::default(), Arc::new(NoLedger)).expect("open");
    let first = repairs.owe(record(1)).await.expect("owed");
    let second = repairs.owe(record(1)).await.expect("owed again");
    assert!(repairs.is_in_flight(&digest(1)));
    let begun = repairs.begun();
    assert!(
        !repairs
            .settle_evicted(&digest(1), begun, &store)
            .await
            .expect("asked")
    );
    drop(first);
    assert!(
        repairs.is_in_flight(&digest(1)),
        "the second write is still committing"
    );
    assert!(
        !repairs
            .settle_evicted(&digest(1), begun, &store)
            .await
            .expect("asked")
    );
    assert_eq!(repairs.open_records(), [record(1)]);
    drop(second);
    assert!(!repairs.is_in_flight(&digest(1)));

    // A write of another blob began owing after `begun` was read: not now, but later.
    let other = repairs.owe(record(2)).await.expect("owed");
    drop(other);
    assert!(
        !repairs
            .settle_evicted(&digest(1), begun, &store)
            .await
            .expect("asked")
    );
    let begun = repairs.begun();
    assert!(
        repairs
            .settle_evicted(&digest(1), begun, &store)
            .await
            .expect("asked")
    );
    assert_eq!(repairs.open_records(), [record(2)]);
}

/// Catches a write that fails to journal its debt leaving its blob marked in flight for ever
/// (the worker would never look at the debt again): the journal fails, the call fails, and
/// the digest is not in flight.
#[tokio::test]
async fn a_debt_that_could_not_be_journalled_is_not_in_flight() {
    let fs = FaultFs::new();
    let repairs = Repairs::open(fs.clone(), Options::default(), Arc::new(NoLedger)).expect("open");
    fs.fail_at(fs.ops(), kbf_store::Fault::Error);
    let answer = repairs.owe(record(1)).await;
    assert!(
        matches!(answer, Err(StorageError::Unavailable(_))),
        "{answer:?}"
    );
    assert!(!repairs.is_in_flight(&digest(1)));
}

/// Catches an abandoned write (its task aborted between the debt and the commit) leaving its
/// blob in flight: the `Pending` it held is dropped with it, so the worker may look at the
/// debt again.
#[tokio::test]
async fn an_abandoned_write_is_no_longer_in_flight() {
    let repairs = Arc::new(
        Repairs::open(FaultFs::new(), Options::default(), Arc::new(NoLedger)).expect("open"),
    );
    let write = tokio::spawn({
        let repairs = Arc::clone(&repairs);
        async move {
            let _pending = repairs.owe(record(3)).await.expect("owed");
            std::future::pending::<()>().await;
        }
    });
    while !repairs.is_in_flight(&digest(3)) {
        tokio::task::yield_now().await;
    }
    write.abort();
    assert!(write.await.expect_err("aborted").is_cancelled());
    assert!(!repairs.is_in_flight(&digest(3)));
}

/// Catches an evicted debt reported dropped when the journal could not write it: the call
/// fails UNAVAILABLE and the debt is still owed.
#[tokio::test]
async fn an_evicted_debt_whose_settling_cannot_be_written_stays_owed() {
    let fs = FaultFs::new();
    let repairs = Repairs::open(fs.clone(), Options::default(), Arc::new(NoLedger)).expect("open");
    drop(repairs.owe(record(1)).await.expect("owed"));
    fs.fail_at(fs.ops(), kbf_store::Fault::Error);
    let answer = repairs
        .settle_evicted(&digest(1), repairs.begun(), &Mem::default())
        .await;
    assert!(
        matches!(answer, Err(StorageError::Unavailable(_))),
        "{answer:?}"
    );
    assert_eq!(repairs.open_records(), [record(1)]);
}

/// Catches a debt dropped for a blob the store holds (a write committed after the worker
/// read it absent), and one dropped when the store could not say: with the blob held, or the
/// store failing, `settle_evicted` leaves the debt; with the blob absent it drops it.
/// Mutants: the store not asked; its error read as "absent".
#[tokio::test]
async fn an_evicted_debt_stays_while_the_store_holds_the_blob_or_cannot_say() {
    let repairs =
        Repairs::open(FaultFs::new(), Options::default(), Arc::new(NoLedger)).expect("open");
    drop(repairs.owe(record(1)).await.expect("owed"));
    let store = Mem::default();
    store
        .blobs
        .lock()
        .expect("lock")
        .insert(digest(1), bytes::Bytes::from_static(b"held"));
    let begun = repairs.begun();
    assert!(
        !repairs
            .settle_evicted(&digest(1), begun, &store)
            .await
            .expect("asked")
    );
    assert_eq!(repairs.open_records(), [record(1)]);

    *store.fail.lock().expect("lock") = Some(StorageError::Internal("the store".into()));
    let answer = repairs.settle_evicted(&digest(1), begun, &store).await;
    assert!(
        matches!(answer, Err(StorageError::Internal(_))),
        "{answer:?}"
    );
    assert_eq!(repairs.open_records(), [record(1)]);

    store.fail.lock().expect("lock").take();
    store.blobs.lock().expect("lock").clear();
    assert!(
        repairs
            .settle_evicted(&digest(1), begun, &store)
            .await
            .expect("asked")
    );
    assert!(repairs.open_records().is_empty());
}
