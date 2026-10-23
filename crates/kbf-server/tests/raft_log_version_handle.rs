//! The version handle of `RaftMetaLog`, the machine version a node reports to its frontend
//! at every heartbeat, over an in-memory raft directory (`FaultFs`) that a test can restart
//! or fail at any operation. `tests/raft_log.rs` holds the rest of the log's tests.

use std::num::NonZeroU64;
use std::sync::atomic::Ordering;

use kbf_front::MetaLogError;
use kbf_log::{FarmLogId, MachineVersion};
use kbf_meta::Retention;
use kbf_server::{RaftLogError, RaftMetaLog};
use kbf_store::{Fault, FaultFs};

type Log = RaftMetaLog<FaultFs>;

fn id(n: u64) -> FarmLogId {
    FarmLogId::new(NonZeroU64::new(n).expect("non-zero"))
}

async fn open_as(fs: &FaultFs, root: &str, new: u64) -> Result<Log, RaftLogError> {
    RaftMetaLog::open(fs.clone(), Retention::default(), root, move || Ok(id(new))).await
}

async fn open(fs: &FaultFs, new: u64) -> Log {
    open_as(fs, "kbf/", new).await.expect("open the log")
}

/// Catches: a version handle that is not the log's version, which a node reports to its
/// frontend at every heartbeat (`Joiner::with_log`). It reads 1 for a new log (defect: a
/// handle that starts at `RUNS`), follows a raise taken after the handle (defect: no store
/// in `raise_machine_version`), stays put for a raise that commits nothing, and for one the
/// disk refuses (defect: the store before the proposal), and reads the raised version of a
/// log opened again (defect: the handle starting at 1 whatever the log replayed).
#[tokio::test(start_paused = true)]
async fn the_version_handle_follows_the_machine_version() {
    let fs = FaultFs::new();
    let log = open(&fs, 8).await;
    let handle = log.version_handle();
    assert_eq!(handle.load(Ordering::Relaxed), MachineVersion::V1.get());
    log.raise_machine_version(MachineVersion::V4)
        .expect("a raise");
    assert_eq!(handle.load(Ordering::Relaxed), MachineVersion::V4.get());
    log.raise_machine_version(MachineVersion::V2)
        .expect("nothing to raise");
    assert_eq!(handle.load(Ordering::Relaxed), MachineVersion::V4.get());
    drop(log);
    let log = open(&fs.restart(), 8).await;
    let handle = log.version_handle();
    assert_eq!(handle.load(Ordering::Relaxed), MachineVersion::V4.get());

    let fs = FaultFs::new();
    let log = open(&fs, 9).await;
    let handle = log.version_handle();
    fs.fail_at(fs.ops(), Fault::Error);
    let refused = log.raise_machine_version(MachineVersion::V2);
    assert!(
        matches!(refused, Err(MetaLogError::Unavailable(_))),
        "{refused:?}"
    );
    assert_eq!(handle.load(Ordering::Relaxed), MachineVersion::V1.get());
}
