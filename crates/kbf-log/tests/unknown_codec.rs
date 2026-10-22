//! What `decode` skips and still refuses, by the rule that every format changes additively
//! (see the crate documentation). Each test states the behaviour it asserts.

use std::num::NonZeroU64;

use kbf_log::{DecodeError, EncodeError, Format, LogCommand, Takeover, decode, encode};
use kbf_meta::{Command, Epoch, Generation, NodeWriter, ObjectId, ServerId, StagedPut, TierId};
use kbf_proto::kbf::log::v1 as proto;
use kbf_types::FarmTime;
use prost::Message;

const UNKNOWN: Result<LogCommand, DecodeError> = Ok(LogCommand::Unknown);

fn bytes(hex: &str) -> Vec<u8> {
    hex::decode(hex).expect("hex")
}

fn object(epoch: u64, seq: u64) -> ObjectId {
    ObjectId::new(Epoch::new(epoch), seq)
}

fn meta_entry(command: proto::meta_command::Command) -> Vec<u8> {
    proto::LogCommand {
        command: Some(proto::log_command::Command::Meta(proto::MetaCommand {
            command: Some(command),
        })),
    }
    .encode_to_vec()
}

fn pobject() -> Option<proto::ObjectId> {
    Some(proto::ObjectId {
        epoch: 1,
        seq: 2,
        ..proto::ObjectId::default()
    })
}

fn unreachable(reason: i32) -> proto::meta_command::Command {
    proto::meta_command::Command::ObjectUnreachable(proto::ObjectUnreachable {
        object: pobject(),
        reason,
    })
}

fn digest(function: i32, hash: Vec<u8>) -> proto::Digest {
    proto::Digest {
        function,
        hash,
        size_bytes: 1,
    }
}

fn touch(blob: proto::Digest) -> proto::meta_command::Command {
    proto::meta_command::Command::Touch(proto::Touch {
        blobs: vec![blob],
        actions: Vec::new(),
    })
}

fn failed_report(failure: i32) -> Vec<u8> {
    proto::LogCommand {
        command: Some(proto::log_command::Command::Report(proto::Report {
            lease: Some(proto::LeaseId::default()),
            outcome: Some(proto::Outcome {
                outcome: Some(proto::outcome::Outcome::Failed(failure)),
            }),
            ..proto::Report::default()
        })),
    }
    .encode_to_vec()
}

/// Asserts that a field no message of this binary names is ignored, at the top level,
/// inside a command and inside a nested message of a command, and that the part this
/// binary knows decodes to the value it would without the field.
#[test]
fn an_unknown_field_in_a_known_command_is_ignored_and_the_known_part_decodes() {
    let tick = LogCommand::Meta(Command::Tick(FarmTime::from_millis(1500)));
    // LogCommand { meta { tick { 1500 } }, field 15 = 1 }
    let top = bytes("0a050a0308dc0b7801");
    assert_eq!(decode(Format::V1, &top), Ok(tick.clone()));
    // Tick { 1500, field 15 = 1 }
    let inner = bytes("0a070a0508dc0b7801");
    assert_eq!(decode(Format::V1, &inner), Ok(tick));
    // ObjectReachable { object { epoch 2, seq 9, field 15 = 1 }, generation 300 }
    let nested = bytes("0a0d420b0a0608021009780110ac02");
    let reachable = LogCommand::Meta(Command::ObjectReachable {
        object: object(2, 9),
        generation: Generation::new(300),
    });
    assert_eq!(decode(Format::V3, &nested), Ok(reachable));
    // Takeover { term 5, farm time 7, field 15 = 1 }
    let control = bytes("2a06080510077801");
    let takeover = LogCommand::Takeover(Takeover {
        term: NonZeroU64::new(5).expect("non-zero"),
        now: FarmTime::from_millis(7),
    });
    assert_eq!(decode(Format::V1, &control), Ok(takeover));
}

/// Asserts that a `oneof` with no arm this binary names decodes as an unknown command,
/// whether it is the command of an entry, the command of a metadata command or the input
/// of a farm batch, and so does an entry with no arm at all.
#[test]
fn an_unknown_oneof_arm_decodes_as_unknown() {
    // `LogCommand` arm 15, empty.
    let top = bytes("7a00");
    // `MetaCommand` arm 21, empty, inside a metadata command.
    let meta = bytes("0a03aa0100");
    // A farm batch holding an input of arm 12, empty.
    let input = bytes("1a041a026200");
    let none = bytes("");
    for (name, entry) in [
        ("a command", top),
        ("a metadata command", meta),
        ("a farm input", input),
        ("no arm at all", none),
    ] {
        assert_eq!(decode(Format::V1, &entry), UNKNOWN, "{name}");
    }
}

/// Asserts that an enum value this binary does not name, or the unspecified zero, in a
/// command it knows, decodes as an unknown command: a mark's reason, a digest's hash
/// function, a failure of a report. The named values still decode to their commands.
#[test]
fn an_unknown_enum_value_decodes_as_unknown() {
    for value in [0, 3, 99] {
        let entry = meta_entry(unreachable(value));
        assert_eq!(decode(Format::V1, &entry), UNKNOWN, "reason {value}");
    }
    for value in [0, 9] {
        let entry = meta_entry(touch(digest(value, vec![7; 32])));
        assert_eq!(decode(Format::V1, &entry), UNKNOWN, "function {value}");
    }
    for value in [0, 99] {
        let entry = failed_report(value);
        assert_eq!(decode(Format::V1, &entry), UNKNOWN, "failure {value}");
    }
    let marked = decode(Format::V1, &meta_entry(unreachable(1)));
    assert!(matches!(marked, Ok(LogCommand::Meta(_))), "{marked:?}");
    let hashed = decode(Format::V1, &meta_entry(touch(digest(1, vec![7; 32]))));
    assert!(matches!(hashed, Ok(LogCommand::Meta(_))), "{hashed:?}");
    let report = decode(Format::V1, &failed_report(1));
    assert!(matches!(report, Ok(LogCommand::Report(_))), "{report:?}");
}

/// Writes `command` under the format it joined and asserts that it decodes to itself
/// under every format this binary knows.
fn reads_under_every_format(joined: Format, command: Command) {
    let entry = LogCommand::Meta(command);
    let written = encode(joined, &entry).expect("encode");
    for n in 1..=5 {
        assert_eq!(
            decode(Format::new(n), &written).as_ref(),
            Ok(&entry),
            "format {n}"
        );
    }
}

/// Asserts that a command from a later format decodes, to the same command, under every
/// earlier committed format: one command of each of formats 2 to 5, written under its
/// own format, read under formats 1 to 5.
#[test]
fn a_command_from_a_later_format_decodes_under_an_earlier_committed_format() {
    let add_tier = Command::AddTier {
        server: ServerId::new(3),
        tier: TierId::new(7),
    };
    reads_under_every_format(Format::V2, add_tier);
    let evict = Command::EvictObjects {
        objects: [object(2, 9)].into(),
    };
    reads_under_every_format(Format::V3, evict);
    let one = NonZeroU64::MIN;
    let set_writer = Command::SetWriter(NodeWriter::new(one, one.saturating_add(1)));
    reads_under_every_format(Format::V4, set_writer);
    let reseat = Command::ReseatStaged {
        tier: TierId::new(2),
        from: object(2, 9),
        to: StagedPut {
            object: object(2, 11),
            end: 300,
            seal: true,
            blobs: Vec::new(),
        },
        stored: false,
    };
    reads_under_every_format(Format::V5, reseat);
}

/// Asserts that bytes that are not a well-formed message are refused as malformed, and
/// not read as an unknown command: a tag cut short, a length past the end of the entry,
/// a known field of the wrong wire type.
#[test]
fn garbage_bytes_still_refuse() {
    for hex in ["ff", "0a05", "0a0b0a", "0801"] {
        let refused = decode(Format::V1, &bytes(hex));
        assert!(
            matches!(refused, Err(DecodeError::Malformed(_))),
            "{hex}: {refused:?}"
        );
    }
}

/// Asserts that a digest that is absent is refused as missing, and a hash that is not 32
/// bytes (empty, short, long) as the wrong length, however the entry came to be written.
#[test]
fn a_missing_hash_is_still_refused() {
    let blob = proto::meta_command::Command::PutBlob(proto::PutBlob {
        digest: None,
        location: Some(proto::Location {
            store: 0,
            object: pobject(),
            offset: 0,
        }),
    });
    let refused = decode(Format::V1, &meta_entry(blob));
    assert_eq!(refused, Err(DecodeError::Missing("PutBlob.digest")));
    for len in [0, 31, 33] {
        let entry = meta_entry(touch(digest(1, vec![7; len])));
        let want = DecodeError::HashLength {
            field: "Touch.blobs",
            len,
        };
        assert_eq!(decode(Format::V1, &entry), Err(want), "{len} bytes");
    }
}

/// Asserts that an unknown command cannot be encoded: its bytes are not kept, so it can
/// not be written back into a log.
#[test]
fn an_unknown_command_cannot_be_written() {
    assert_eq!(
        encode(Format::V5, &LogCommand::Unknown),
        Err(EncodeError::Unknown)
    );
}
