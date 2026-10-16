//! [`Repairs`] over a disk that panics, and the ledger that tells no one.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kbf_store::{FaultFile, FaultFs, Fs, FsFile};

use super::{digest, record};
use crate::StorageError;
use crate::repair::{NoLedger, Options, RepairLedger, Repairs};

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
    repairs.owe(record(1)).await.expect("owed");
    armed.store(true, Ordering::SeqCst);
    let answer = repairs.owe(record(2)).await;
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
