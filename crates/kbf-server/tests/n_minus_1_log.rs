//! The metadata log across builds that run different machine versions: every format changes
//! additively, so a release starts on the log the release before it wrote, and the release
//! before it starts on a log this one wrote (docs/design/kbf-cicd.md, section 9). The farm
//! tests of `kbf-integration-test` do this with two real binaries once a release exists; these
//! run in every pull request. `RaftMetaLog::open_running` stands for the older build in one
//! respect: it opens the log as a binary that runs only machine versions up to the one given,
//! which skips the raises past it and keeps the format it had, the way a rollback does. It is
//! this build's machine otherwise, so it knows and applies every command of this build: what
//! these tests count as skipped is the raises past the version, and not the commands an older
//! build lacks. Entries a binary does not know are the subject of `raft_log_unknown.rs`.
//!
//! This build raises no log. The logs an earlier build left, which hold the raises it
//! wrote, are made here with `raise_machine_version`; the logs this build writes are made
//! with none, and hold no `MachineVersion` entry.
//!
//! The log is an in-memory raft directory (`FaultFs`). Between two opens it is restarted.

use std::num::NonZeroU64;

use kbf_front::MetaLog;
use kbf_log::{FarmLogId, MachineVersion};
use kbf_meta::{
    ActionRecord, Applied, Closure, Command, Epoch, Generation, LegacyWriter, Location, MetaState,
    NodeWriter, ObjectId, Retention, Role, ServerId, StagedPut, StoreId, TierId, Touch,
    UnreachableReason, Writer,
};
use kbf_server::RaftMetaLog;
use kbf_store::FaultFs;
use kbf_types::{Digest, DigestFunction, FarmTime};

type Log = RaftMetaLog<FaultFs>;

fn nz(n: u64) -> NonZeroU64 {
    NonZeroU64::new(n).expect("non-zero")
}

fn d(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
}

fn object(epoch: u64, seq: u64) -> ObjectId {
    ObjectId::new(Epoch::new(epoch), seq)
}

fn location(object: ObjectId, offset: u64) -> Location {
    Location {
        store: StoreId::new(1),
        object,
        offset,
    }
}

fn staged(seq: u64, seal: bool) -> StagedPut {
    StagedPut {
        object: object(1, seq),
        end: 300,
        seal,
        blobs: vec![(d(7), 0)],
    }
}

/// The log in `fs`, restarted first, opened by a binary that runs machine versions up to
/// `runs`. A new log is made if `fs` is empty.
async fn open_running(fs: &mut FaultFs, runs: MachineVersion) -> Log {
    *fs = fs.restart();
    RaftMetaLog::open_running(
        fs.clone(),
        Retention::default(),
        "kbf/",
        || Ok(FarmLogId::new(nz(7))),
        runs,
    )
    .await
    .expect("open the log")
}

/// The metadata state the log has applied.
async fn state_of(log: &Log) -> MetaState {
    log.query(|state| state.clone()).await.expect("a query")
}

async fn epoch(log: &Log) -> u64 {
    match log.commit(Command::AllocEpoch).await {
        Ok(Applied::Epoch(e)) => e.get(),
        other => panic!("AllocEpoch answered {other:?}"),
    }
}

/// The commands of machine version 1 (format 1): the ones the first release wrote.
fn version_1() -> Vec<Command> {
    let blobs = vec![
        (d(4), location(object(1, 2), 0)),
        (d(5), location(object(1, 2), 100)),
    ];
    let record = ActionRecord {
        result: d(3),
        closure: Closure::from_iter([d(4)]),
    };
    vec![
        Command::Tick(FarmTime::from_millis(1500)),
        Command::AllocEpoch,
        Command::PutBlob {
            digest: d(3),
            location: location(object(1, 1), 0),
        },
        Command::PutBlobs(blobs),
        Command::PutAction {
            role: Role::Daemon,
            action: d(6),
            record,
        },
        Command::Touch(Touch {
            blobs: [d(3)].into(),
            actions: [d(6)].into(),
        }),
        Command::ObjectUnreachable {
            object: object(1, 2),
            reason: UnreachableReason::Missing,
        },
        Command::ObjectReachable {
            object: object(1, 2),
            generation: Generation::new(1),
        },
        Command::Collect,
    ]
}

/// The write tier's commands, which joined with machine version 2.
fn version_2() -> Vec<Command> {
    vec![
        Command::AddTier {
            server: ServerId::new(1),
            tier: TierId::new(1),
        },
        Command::PutStaged {
            tier: TierId::new(1),
            objects: vec![staged(3, false)],
        },
        Command::SealStaged {
            object: object(1, 3),
        },
        Command::ObjectFlushed {
            object: object(1, 3),
        },
        Command::FlushConflict {
            object: object(1, 3),
        },
    ]
}

/// The commands of machine version 3: the eviction of objects.
fn version_3() -> Vec<Command> {
    let evict = Command::EvictObjects {
        objects: [object(1, 2)].into(),
    };
    vec![evict]
}

/// The commands of machine version 4: node writers and the rebuild of the index.
fn version_4() -> Vec<Command> {
    let legacy = LegacyWriter::new(nz(9), Epoch::new(5));
    vec![
        Command::SetWriter(NodeWriter::new(nz(7), nz(2))),
        Command::AdoptSegment {
            object: ObjectId::of(Writer::Legacy(legacy), 3),
            entries: vec![(d(8), 0)],
        },
        Command::RebuildProgress {
            after: "a".to_owned(),
        },
        Command::RebuildDone,
    ]
}

/// The command of machine version 5, the newest: the reseat of a staged object.
fn version_5() -> Vec<Command> {
    let reseat = Command::ReseatStaged {
        tier: TierId::new(1),
        from: object(1, 3),
        to: staged(4, true),
        stored: false,
    };
    vec![reseat]
}

/// Each machine version with the commands that joined with it.
fn stages() -> Vec<(MachineVersion, Vec<Command>)> {
    vec![
        (MachineVersion::V1, version_1()),
        (MachineVersion::V2, version_2()),
        (MachineVersion::V3, version_3()),
        (MachineVersion::V4, version_4()),
        (MachineVersion::V5, version_5()),
    ]
}

/// Writes a new log in `fs` as an earlier build left it: it holds one entry of every
/// metadata command this build writes, the log is raised through machine versions 2 to 5, and
/// each version's commands are committed after its raise. It ends at version 5.
async fn write_every_command(fs: &mut FaultFs) {
    let log = open_running(fs, MachineVersion::V5).await;
    for (version, commands) in stages() {
        log.raise_machine_version(version).expect("a raise");
        for command in commands {
            let answered = log.commit(command.clone()).await;
            assert!(answered.is_ok(), "{command:?}: {answered:?}");
        }
    }
    assert_eq!(log.machine_version(), MachineVersion::V5);
}

/// Writes a new log in `fs` as this build does: one entry of every metadata command this
/// build writes, committed to a log that is never raised. It stays at version 1.
async fn write_every_command_unraised(fs: &mut FaultFs) {
    let log = open_running(fs, MachineVersion::V5).await;
    for (_, commands) in stages() {
        for command in commands {
            let answered = log.commit(command.clone()).await;
            assert!(answered.is_ok(), "{command:?}: {answered:?}");
        }
    }
    assert_eq!(log.machine_version(), MachineVersion::V1);
}

/// Asserts that this build writes a log holding one entry of every metadata command it
/// writes with no `MachineVersion` entry in it, and that an older binary opens that log: each
/// binary that runs only machine version 1, 2, 3 or 4 skips nothing (there is no raise to
/// skip), stays at version 1, holds the same metadata state as this build and finds the log's
/// first command; and that this build then opens the same log again at version 1 with
/// nothing skipped.
#[tokio::test]
async fn this_build_writes_every_command_with_no_raise_and_an_older_binary_opens_it() {
    let mut fs = FaultFs::new();
    write_every_command_unraised(&mut fs).await;
    let newest = open_running(&mut fs, MachineVersion::V5).await;
    assert_eq!(newest.skipped_entries(), 0);
    assert_eq!(newest.machine_version(), MachineVersion::V1);
    let state = state_of(&newest).await;
    drop(newest);
    let older = [
        MachineVersion::V1,
        MachineVersion::V2,
        MachineVersion::V3,
        MachineVersion::V4,
    ];
    for runs in older {
        let log = open_running(&mut fs, runs).await;
        let n = runs.get();
        assert_eq!(log.skipped_entries(), 0, "runs {n}");
        assert_eq!(log.machine_version(), MachineVersion::V1, "runs {n}");
        assert_eq!(log.bootstraps(), 1, "runs {n}");
        assert_eq!(state_of(&log).await, state, "runs {n}");
        drop(log);
    }
    let log = open_running(&mut fs, MachineVersion::V5).await;
    assert_eq!(log.skipped_entries(), 0);
    assert_eq!(log.machine_version(), MachineVersion::V1);
    assert_eq!(state_of(&log).await, state);
}

/// Asserts that a binary that runs only machine version 1, 2, 3 or 4 opens a log an earlier
/// build left, written with one entry of every command this build writes (raised through
/// versions 2 to 5): it counts
/// each raise past the version it runs as skipped (the raises only: it applies every command),
/// stays at its version, holds the same metadata state as this build and finds the log's first
/// command; and that this build then opens the same log again at version 5 with nothing
/// skipped.
#[tokio::test]
async fn an_older_binary_opens_a_log_that_holds_every_command_this_build_writes() {
    let mut fs = FaultFs::new();
    write_every_command(&mut fs).await;
    let newest = open_running(&mut fs, MachineVersion::V5).await;
    assert_eq!(newest.skipped_entries(), 0);
    let state = state_of(&newest).await;
    drop(newest);
    let older = [
        (MachineVersion::V1, 4),
        (MachineVersion::V2, 3),
        (MachineVersion::V3, 2),
        (MachineVersion::V4, 1),
    ];
    for (runs, raises_skipped) in older {
        let log = open_running(&mut fs, runs).await;
        let n = runs.get();
        assert_eq!(log.skipped_entries(), raises_skipped, "runs {n}");
        assert_eq!(log.machine_version(), runs, "runs {n}");
        assert_eq!(log.bootstraps(), 1, "runs {n}");
        assert_eq!(state_of(&log).await, state, "runs {n}");
        drop(log);
    }
    let log = open_running(&mut fs, MachineVersion::V5).await;
    assert_eq!(log.skipped_entries(), 0);
    assert_eq!(log.machine_version(), MachineVersion::V5);
    assert_eq!(state_of(&log).await, state);
}

/// Asserts that a log goes both ways between this build and a binary that runs only machine
/// version 1: this build opens what that binary wrote and holds the same state; after this
/// build committed a tier command with no raise, the older binary opens it, skips nothing,
/// applies the tier command and appends an epoch; and this build then opens the log again,
/// applies that epoch and continues from it, the log still at version 1 throughout.
#[tokio::test]
async fn a_log_goes_both_ways_between_this_build_and_an_older_binary() {
    let mut fs = FaultFs::new();
    let older = open_running(&mut fs, MachineVersion::V1).await;
    for command in version_1() {
        let answered = older.commit(command.clone()).await;
        assert!(answered.is_ok(), "{command:?}: {answered:?}");
    }
    let written = state_of(&older).await;
    drop(older);

    let newest = open_running(&mut fs, MachineVersion::V5).await;
    assert_eq!(newest.skipped_entries(), 0);
    assert_eq!(state_of(&newest).await, written);
    let tier = Command::AddTier {
        server: ServerId::new(1),
        tier: TierId::new(1),
    };
    assert_eq!(newest.commit(tier).await, Ok(Applied::Tier(Ok(()))));
    assert_eq!(newest.machine_version(), MachineVersion::V1);
    let committed = state_of(&newest).await;
    drop(newest);

    let older = open_running(&mut fs, MachineVersion::V1).await;
    assert_eq!(older.skipped_entries(), 0);
    assert_eq!(older.machine_version(), MachineVersion::V1);
    assert_eq!(state_of(&older).await, committed);
    let appended = epoch(&older).await;
    drop(older);

    let newest = open_running(&mut fs, MachineVersion::V5).await;
    assert_eq!(newest.skipped_entries(), 0);
    assert_eq!(newest.machine_version(), MachineVersion::V1);
    assert_eq!(epoch(&newest).await, appended + 1);
}
