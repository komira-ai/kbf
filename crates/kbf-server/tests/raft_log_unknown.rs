//! `RaftMetaLog` over entries its binary does not apply (a command it does not know, a
//! control command, a raise past the machine versions it runs), in an in-memory raft
//! directory (`FaultFs`) filled by hand: each is skipped and counted, and the entries
//! after it apply. `tests/raft_log.rs` holds the rest of the log's tests.

use std::num::{NonZeroU32, NonZeroU64};
use std::time::UNIX_EPOCH;

use kbf_front::{MetaLog, MetaLogError};
use kbf_log::{
    Checkpoint, FarmBatch, FarmInput, FarmLogId, Format, LogCommand, MachineVersion, Report,
    Takeover,
};
use kbf_meta::{Applied, Command, Retention, ServerId, TierId};
use kbf_raft::{Entry, LogId, Payload, Storage};
use kbf_server::{RaftLogError, RaftMetaLog};
use kbf_store::{FaultFs, Options, Store};
use kbf_types::{Failure, FarmTime, LeaseId, OperationId, Outcome};

type Log = RaftMetaLog<FaultFs>;

/// A metadata command in `oneof` arm 21, which no message names, empty.
const UNKNOWN: [u8; 5] = [0x0a, 0x03, 0xaa, 0x01, 0x00];

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

async fn epoch(log: &Log) -> u64 {
    match log.commit(Command::AllocEpoch).await {
        Ok(Applied::Epoch(e)) => e.get(),
        other => panic!("AllocEpoch answered {other:?}"),
    }
}

fn encoded(command: &LogCommand) -> Vec<u8> {
    kbf_log::encode(Format::V1, command).expect("encode")
}

fn alloc_epoch() -> Vec<u8> {
    encoded(&LogCommand::Meta(Command::AllocEpoch))
}

/// Appends `payloads` to the log `fs` holds, as the next entries of its last term;
/// returns the index of the first.
fn append_all(fs: &FaultFs, payloads: Vec<Vec<u8>>) -> u64 {
    let (mut store, held) = Store::open(fs.clone(), Options::default()).expect("the store");
    let last = held.entries.last().expect("entries").id;
    let mut index = last.index;
    let entries: Vec<_> = payloads
        .into_iter()
        .map(|bytes| {
            index = index.next();
            Entry {
                id: LogId::new(last.term, index),
                payload: Payload::Command(bytes),
            }
        })
        .collect();
    store.write_entries(&entries).expect("append");
    last.index.0 + 1
}

/// One command of each control kind, which the metadata log does not use.
fn control_commands() -> [LogCommand; 4] {
    let farm = FarmBatch {
        now: FarmTime::default(),
        wall: UNIX_EPOCH,
        inputs: vec![FarmInput::Tick],
    };
    let report = Report {
        now: FarmTime::default(),
        operation: OperationId(1),
        lease: LeaseId::new(5, 0),
        outcome: Outcome::Failed(Failure::Invalid),
        action_entry: None,
    };
    [
        LogCommand::Farm(farm),
        LogCommand::Report(report),
        LogCommand::Takeover(Takeover {
            term: NonZeroU64::MIN,
            now: FarmTime::from_millis(1),
        }),
        LogCommand::Checkpoint(Checkpoint {
            index: 1,
            digest: [0; 32],
        }),
    ]
}

fn add_tier() -> Command {
    Command::AddTier {
        server: ServerId::new(1),
        tier: TierId::new(1),
    }
}

/// Asserts that a command whose `oneof` arm this binary does not name does not stop the
/// log: the open succeeds, the entry is counted as skipped, and the command after it
/// applies (the next epoch is the one after the epoch that command allocated).
#[tokio::test]
async fn entries_after_a_skipped_unknown_command_still_apply() {
    let fs = FaultFs::new();
    drop(open(&fs, 2).await);
    let dir = fs.restart();
    append_all(&dir, vec![UNKNOWN.to_vec(), alloc_epoch()]);
    let log = open(&dir.restart(), 2).await;
    assert_eq!(log.skipped_entries(), 1);
    assert_eq!(epoch(&log).await, 2);
}

/// Asserts that each kind of control command (`Farm`, `Report`, `Takeover` and
/// `Checkpoint`) in the metadata log is skipped and counted, not applied and not
/// refused, and that the command after it applies.
#[tokio::test]
async fn a_control_command_in_the_metadata_log_is_skipped() {
    for command in control_commands() {
        let fs = FaultFs::new();
        drop(open(&fs, 2).await);
        let dir = fs.restart();
        append_all(&dir, vec![encoded(&command), alloc_epoch()]);
        let log = open(&dir.restart(), 2).await;
        assert_eq!(log.skipped_entries(), 1, "{command:?}");
        assert_eq!(epoch(&log).await, 2, "{command:?}");
    }
}

/// Asserts that a raise to a machine version this binary does not know is skipped and
/// counted: the log stays at version 1, and the command after the raise applies.
#[tokio::test]
async fn a_raise_to_a_version_this_binary_does_not_know_is_skipped() {
    let v6 = MachineVersion::new(NonZeroU32::new(6).expect("non-zero"));
    let fs = FaultFs::new();
    drop(open(&fs, 2).await);
    let dir = fs.restart();
    let raise = encoded(&LogCommand::MachineVersion(v6));
    append_all(&dir, vec![raise, alloc_epoch()]);
    let log = open(&dir.restart(), 2).await;
    assert_eq!(log.skipped_entries(), 1);
    assert_eq!(log.machine_version(), MachineVersion::V1);
    assert_eq!(epoch(&log).await, 2);
}

/// Asserts that a raise to a machine version past what the binary runs is refused and the
/// log stays where it was: a binary that runs only version 1 is refused a raise to version
/// 2 with `Unavailable`, and its log is still at version 1.
#[tokio::test]
async fn a_raise_past_what_the_binary_runs_is_refused() {
    let log = RaftMetaLog::open_running(
        FaultFs::new(),
        Retention::default(),
        "kbf/",
        || Ok(id(2)),
        MachineVersion::V1,
    )
    .await
    .expect("a version-1 binary opens a new log");
    let refused = log.raise_machine_version(MachineVersion::V2);
    assert!(
        matches!(refused, Err(MetaLogError::Unavailable(_))),
        "{refused:?}"
    );
    assert_eq!(log.machine_version(), MachineVersion::V1);
}

/// Asserts that a binary that runs only machine version 1 opens a log raised to version
/// 2: it skips the raise, stays at version 1, applies the tier command committed after
/// the raise, and the same log opens again under this binary at version 2 with nothing
/// skipped. And that a binary asked to run a version it does not know is refused.
#[tokio::test]
async fn a_log_raised_past_what_a_binary_runs_opens_at_the_version_it_runs() {
    let fs = FaultFs::new();
    let log = open(&fs, 4).await;
    log.raise_machine_version(MachineVersion::V2)
        .expect("a raise");
    assert_eq!(log.commit(add_tier()).await, Ok(Applied::Tier(Ok(()))));
    drop(log);
    let older = RaftMetaLog::open_running(
        fs.restart(),
        Retention::default(),
        "kbf/",
        || Ok(id(4)),
        MachineVersion::V1,
    )
    .await
    .expect("a version-1 binary opens the log");
    assert_eq!(older.skipped_entries(), 1);
    assert_eq!(older.machine_version(), MachineVersion::V1);
    assert_eq!(
        older.query(|m| m.tier(ServerId::new(1))).await,
        Ok(Some(TierId::new(1)))
    );
    drop(older);
    let v6 = MachineVersion::new(NonZeroU32::new(6).expect("non-zero"));
    let unknown =
        RaftMetaLog::open_running(fs.restart(), Retention::default(), "kbf/", || Ok(id(4)), v6)
            .await;
    assert!(
        matches!(&unknown, Err(RaftLogError::Runs(v)) if *v == v6),
        "{unknown:?}"
    );
    let log = open(&fs.restart(), 4).await;
    assert_eq!(log.machine_version(), MachineVersion::V2);
    assert_eq!(log.skipped_entries(), 0);
}

/// Asserts that the skipped count rises by one for each skipped entry: a log with none
/// reads zero, and after each of three starts that each add an entry this binary does not
/// know, it reads one more.
#[tokio::test]
async fn the_skipped_count_rises_with_each_skipped_entry() {
    let fs = FaultFs::new();
    let log = open(&fs, 2).await;
    assert_eq!(log.skipped_entries(), 0);
    drop(log);
    let mut counts = Vec::new();
    for _ in 0..3 {
        let dir = fs.restart();
        append_all(&dir, vec![UNKNOWN.to_vec()]);
        let log = open(&dir.restart(), 2).await;
        counts.push(log.skipped_entries());
        drop(log);
    }
    assert_eq!(counts, [1, 2, 3]);
}

/// Asserts that bytes that are not a well-formed message still stop the open after an
/// entry that was skipped: the refusal names the malformed entry, not the skipped one.
#[tokio::test]
async fn a_malformed_entry_after_a_skipped_one_still_refuses_the_open() {
    let fs = FaultFs::new();
    drop(open(&fs, 2).await);
    let dir = fs.restart();
    let first = append_all(&dir, vec![UNKNOWN.to_vec(), vec![0xff], alloc_epoch()]);
    let refused = refusal(&dir.restart(), 2).await;
    assert!(
        matches!(&refused, RaftLogError::Decode { index, .. } if *index == first + 1),
        "{refused:?}"
    );
}
