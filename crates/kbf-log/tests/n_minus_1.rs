//! The codec of the replicated log across formats (every format changes additively, so a
//! release starts on the data the release before it wrote, and the release before it starts on
//! data this one wrote: docs/design/kbf-cicd.md, section 9). The farm tests of
//! `kbf-integration-test` do this with two real binaries once a release exists, and
//! `kbf-server/tests/n_minus_1_log.rs` opens a log as an older machine version; these run in
//! every pull request, with the one binary there is:
//!
//! - the fixture holds one entry of every kind this build writes, the newest included. The
//!   match in `kind` has no wildcard, so a kind added without a place in it does not compile;
//!   the kind then takes the next number, `KINDS` is raised with it, and the first test fails
//!   until the fixture holds an entry of it;
//! - `decode` reads each entry the same under every format this build knows, 1 to 5: the
//!   format number changes nothing in the reading;
//! - a reader that does not know an entry skips it and reads on, and a field it does not
//!   know after an entry changes nothing.
//!
//! `golden_corpus.rs` holds the other half: entries earlier builds wrote, read by this one.

use std::num::NonZeroU64;
use std::time::UNIX_EPOCH;

use kbf_log::{
    Bootstrap, Checkpoint, DecodeError, FarmBatch, FarmInput, FarmLogId, Format, LogCommand,
    MachineVersion, Report, Takeover, decode, encode,
};
use kbf_meta::{
    ActionRecord, Closure, Command, Epoch, Generation, Location, NodeWriter, ObjectId, Role,
    ServerId, StagedPut, StoreId, TierId, Touch, UnreachableReason, Writer,
};
use kbf_types::{Digest, DigestFunction, Failure, FarmTime, LeaseId, OperationId, Outcome};

/// A field after an entry that no message of this build names: number 1000, a varint, 1.
const LATER_FIELD: [u8; 3] = [0xc0, 0x3e, 0x01];

fn d(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
}

fn nz(n: u64) -> NonZeroU64 {
    NonZeroU64::new(n).expect("not zero")
}

fn object(epoch: u64, seq: u64) -> ObjectId {
    ObjectId::new(Epoch::new(epoch), seq)
}

fn location(seq: u64, offset: u64) -> Location {
    Location {
        store: StoreId::new(1),
        object: object(2, seq),
        offset,
    }
}

fn staged(seq: u64, seal: bool) -> StagedPut {
    StagedPut {
        object: object(2, seq),
        end: 300,
        seal,
        blobs: vec![(d(3), 0), (d(4), 100)],
    }
}

fn hex_bytes(text: &str) -> Vec<u8> {
    hex::decode(text).expect("hex")
}

/// One metadata command of every kind, the ones that joined with each format in turn.
fn meta_commands() -> Vec<Command> {
    let blobs = vec![(d(3), location(7, 0)), (d(4), location(7, 100))];
    let record = ActionRecord {
        result: d(4),
        closure: Closure::from_iter([d(5)]),
    };
    let node = NodeWriter::new(nz(7), nz(2));
    vec![
        Command::Tick(FarmTime::from_millis(1500)),
        Command::AllocEpoch,
        Command::PutBlob {
            digest: d(3),
            location: location(7, 300),
        },
        Command::PutBlobs(blobs),
        Command::PutAction {
            role: Role::Daemon,
            action: d(3),
            record,
        },
        Command::Touch(Touch {
            blobs: [d(3)].into(),
            actions: [d(4)].into(),
        }),
        Command::ObjectUnreachable {
            object: object(2, 9),
            reason: UnreachableReason::Corrupt,
        },
        Command::ObjectReachable {
            object: object(2, 9),
            generation: Generation::new(300),
        },
        Command::Collect,
        Command::AddTier {
            server: ServerId::new(3),
            tier: TierId::new(7),
        },
        Command::PutStaged {
            tier: TierId::new(7),
            objects: vec![staged(9, true)],
        },
        Command::SealStaged {
            object: object(2, 9),
        },
        Command::ObjectFlushed {
            object: object(2, 9),
        },
        Command::FlushConflict {
            object: object(2, 9),
        },
        Command::EvictObjects {
            objects: [object(2, 9)].into(),
        },
        Command::SetWriter(node),
        Command::AdoptSegment {
            object: ObjectId::of(Writer::Node(node), 3),
            entries: vec![(d(5), 7)],
        },
        Command::RebuildProgress {
            after: "a".to_owned(),
        },
        Command::RebuildDone,
        Command::ReseatStaged {
            tier: TierId::new(7),
            from: object(2, 9),
            to: staged(11, true),
            stored: false,
        },
    ]
}

/// One entry of every other kind: the log's first command, a farm batch, a worker's report,
/// a new leader's first command, a determinism check and a raise of the machine version.
fn other_entries() -> Vec<LogCommand> {
    let bootstrap = Bootstrap {
        log_id: FarmLogId::new(nz(1)),
        store_prefix: "kbf/0000000000000001/".to_owned(),
    };
    let farm = FarmBatch {
        now: FarmTime::from_millis(1),
        wall: UNIX_EPOCH,
        inputs: vec![FarmInput::Tick],
    };
    let report = Report {
        now: FarmTime::from_millis(1),
        operation: OperationId(3),
        lease: LeaseId::new(1, 2),
        outcome: Outcome::Failed(Failure::Timeout),
        action_entry: None,
    };
    let takeover = Takeover {
        term: nz(7),
        now: FarmTime::from_millis(9),
    };
    let checkpoint = Checkpoint {
        index: 5,
        digest: [0xab; 32],
    };
    vec![
        LogCommand::Bootstrap(bootstrap),
        LogCommand::Farm(farm),
        LogCommand::Report(report),
        LogCommand::Takeover(takeover),
        LogCommand::Checkpoint(checkpoint),
        LogCommand::MachineVersion(MachineVersion::V5),
    ]
}

/// Every kind of entry this build writes, one entry each.
fn every_kind() -> Vec<LogCommand> {
    let mut all = Vec::new();
    all.extend(meta_commands().into_iter().map(LogCommand::Meta));
    all.extend(other_entries());
    all
}

/// How many kinds of entry this build writes: the numbers `kind` gives are below it.
const KINDS: usize = 26;

/// The place of an entry's kind among the kinds. Nothing in the match is a wildcard: a kind
/// added to the log without a place here does not compile. Its place is the next number, and
/// raising `KINDS` to hold it makes the first test fail until the fixture has an entry of it.
fn kind(command: &LogCommand) -> usize {
    match command {
        LogCommand::Meta(Command::Tick(_)) => 0,
        LogCommand::Meta(Command::AllocEpoch) => 1,
        LogCommand::Meta(Command::PutBlob { .. }) => 2,
        LogCommand::Meta(Command::PutBlobs(_)) => 3,
        LogCommand::Meta(Command::PutAction { .. }) => 4,
        LogCommand::Meta(Command::Touch(_)) => 5,
        LogCommand::Meta(Command::ObjectUnreachable { .. }) => 6,
        LogCommand::Meta(Command::ObjectReachable { .. }) => 7,
        LogCommand::Meta(Command::Collect) => 8,
        LogCommand::Meta(Command::AddTier { .. }) => 9,
        LogCommand::Meta(Command::PutStaged { .. }) => 10,
        LogCommand::Meta(Command::SealStaged { .. }) => 11,
        LogCommand::Meta(Command::ObjectFlushed { .. }) => 12,
        LogCommand::Meta(Command::FlushConflict { .. }) => 13,
        LogCommand::Meta(Command::ReseatStaged { .. }) => 14,
        LogCommand::Meta(Command::EvictObjects { .. }) => 15,
        LogCommand::Meta(Command::SetWriter(_)) => 16,
        LogCommand::Meta(Command::AdoptSegment { .. }) => 17,
        LogCommand::Meta(Command::RebuildProgress { .. }) => 18,
        LogCommand::Meta(Command::RebuildDone) => 19,
        LogCommand::Bootstrap(_) => 20,
        LogCommand::Farm(_) => 21,
        LogCommand::Report(_) => 22,
        LogCommand::Takeover(_) => 23,
        LogCommand::Checkpoint(_) => 24,
        LogCommand::MachineVersion(_) => 25,
        LogCommand::Unknown => unreachable!("an entry this build writes is never unknown"),
    }
}

/// Asserts that the fixture holds an entry of every kind this build writes, each kind once:
/// the twenty metadata commands, the first command of a log, a farm batch, a report, a
/// takeover, a checkpoint and a raise of the machine version.
#[test]
fn the_fixture_holds_one_entry_of_every_kind() {
    let mut seen = [0; KINDS];
    for command in every_kind() {
        seen[kind(&command)] += 1;
    }
    assert_eq!(seen, [1; KINDS]);
}

/// Asserts that `decode` reads an entry of every kind, written under the newest format, as the
/// entry that was written, under each format this build knows, from the first to the newest.
/// It does not stand for an older build: a command an older build lacks is the unknown entry
/// of the last test.
#[test]
fn decode_reads_every_entry_the_same_under_every_known_format() {
    for command in every_kind() {
        let written = encode(Format::V5, &command).expect("encode");
        for n in 1..=5 {
            let read = decode(Format::new(n), &written);
            assert_eq!(read.as_ref(), Ok(&command), "format {n}");
        }
    }
}

/// Asserts that a field no message of this build names, after an entry of any kind, changes
/// nothing: the entry decodes to the value it was written from.
#[test]
fn a_field_a_later_build_adds_after_an_entry_is_skipped() {
    for command in every_kind() {
        let mut bytes = encode(Format::V5, &command).expect("encode");
        bytes.extend_from_slice(&LATER_FIELD);
        let read = decode(Format::V5, &bytes);
        assert_eq!(read.as_ref(), Ok(&command), "{command:?}");
    }
}

/// Asserts that a reader goes on past the entries it does not know: in a log of five entries
/// whose second is a command of the log that no message of this build names and whose fourth
/// is a metadata command that none names, those two read as unknown and the three around them
/// read as written.
#[test]
fn a_log_with_entries_this_build_does_not_know_reads_to_its_end() {
    let kinds = every_kind();
    let (first, second, third) = (&kinds[0], &kinds[1], &kinds[2]);
    let log = [
        encode(Format::V5, first).expect("encode"),
        hex_bytes("7a00"),
        encode(Format::V5, second).expect("encode"),
        hex_bytes("0a03aa0100"),
        encode(Format::V5, third).expect("encode"),
    ];
    let read: Vec<_> = log.iter().map(|b| decode(Format::V5, b)).collect();
    let unknown: Result<LogCommand, DecodeError> = Ok(LogCommand::Unknown);
    let want = [
        Ok(first.clone()),
        unknown.clone(),
        Ok(second.clone()),
        unknown,
        Ok(third.clone()),
    ];
    assert_eq!(read, want);
}
