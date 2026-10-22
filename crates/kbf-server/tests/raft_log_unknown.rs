//! `RaftMetaLog` over an entry whose command this binary does not know, in an in-memory
//! raft directory (`FaultFs`) filled by hand. `tests/raft_log.rs` holds the rest of the
//! log's tests.

use std::num::NonZeroU64;

use kbf_log::{DecodeError, FarmLogId};
use kbf_meta::Retention;
use kbf_raft::{Entry, LogId, Payload, Storage};
use kbf_server::{RaftLogError, RaftMetaLog};
use kbf_store::{FaultFs, Options, Store};

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

/// The error of a start over `fs` that must be refused.
async fn refusal(fs: &FaultFs, new: u64) -> RaftLogError {
    match open_as(fs, "kbf/", new).await {
        Ok(log) => panic!("opened {log:?}"),
        Err(e) => e,
    }
}

/// Asserts that a command whose `oneof` arm this binary does not name, at the end of the
/// log, refuses the open at its index and is named an unknown command: this version
/// skips nothing yet.
#[tokio::test]
async fn a_command_this_binary_does_not_know_refuses_the_open() {
    let fs = FaultFs::new();
    drop(open(&fs, 2).await);
    let dir = fs.restart();
    let (mut store, held) = Store::open(dir.clone(), Options::default()).expect("the store");
    let last = held.entries.last().expect("entries").id;
    // A metadata command in arm 21, which no message names, empty.
    let unknown = vec![0x0a, 0x03, 0xaa, 0x01, 0x00];
    let entry = Entry {
        id: LogId::new(last.term, last.index.next()),
        payload: Payload::Command(unknown),
    };
    store.write_entries(&[entry]).expect("append");
    drop(store);
    let refused = refusal(&dir.restart(), 2).await;
    assert!(
        matches!(
            &refused,
            RaftLogError::Decode { index, source: DecodeError::UnknownCommand("LogCommand") }
                if *index == last.index.0 + 1
        ),
        "{refused:?}"
    );
}
