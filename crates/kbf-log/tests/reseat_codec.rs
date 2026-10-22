//! The format-5 encoding of `ReseatStaged` (docs/design/write-path.md, section 6.4).
//!
//! Catches:
//! - an encoding that changes under a committed log (golden bytes);
//! - a field lost or swapped in a round trip (`stored`, the order of the records);
//! - a required field read as a default when absent.

use kbf_log::{DecodeError, EncodeError, Format, LogCommand, decode, encode};
use kbf_meta::{Command, Epoch, ObjectId, StagedPut, TierId};
use kbf_proto::kbf::log::v1 as proto;
use kbf_types::{Digest, DigestFunction};
use prost::Message;
use proto::meta_command::Command as P;

fn d(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
}

fn object(epoch: u64, seq: u64) -> ObjectId {
    ObjectId::new(Epoch::new(epoch), seq)
}

fn reseat(stored: bool) -> Command {
    Command::ReseatStaged {
        tier: TierId::new(2),
        from: object(2, 9),
        to: StagedPut {
            object: object(2, 11),
            end: 300,
            seal: true,
            blobs: vec![(d(4), 0), (d(3), 100)],
        },
        stored,
    }
}

fn enc(format: Format, command: &Command) -> Result<Vec<u8>, EncodeError> {
    encode(format, &LogCommand::Meta(command.clone()))
}

fn dec(format: Format, bytes: &[u8]) -> Result<Command, DecodeError> {
    match decode(format, bytes)? {
        LogCommand::Meta(c) => Ok(c),
        other => panic!("not a metadata command: {other:?}"),
    }
}

/// Catches: a field renumbered or retyped, or the command moved off MetaCommand field 20.
#[test]
fn the_command_has_its_golden_bytes() {
    // LogCommand.meta (0a) { MetaCommand.reseat_staged (field 20: a2 01) {
    //   tier 2, from {epoch 2, seq 9},
    //   to { object {epoch 2, seq 11}, end 300, seal, blobs [d(4) at 0 (offset omitted),
    //        d(3) at 100] },
    //   stored } }
    let want = concat!(
        "0a70a2016d0802",
        "1204080210091a610a040802100b10ac0218012228",
        "0a26080112200404040404040404040404040404040404040404040404040404040404040404",
        "1804222a0a26080112200303030303030303030303030303030303030303030303030303030303",
        "030303180310642001",
    );
    let bytes = enc(Format::V5, &reseat(true)).expect("encoded");
    assert_eq!(hex::encode(&bytes), want);
    assert_eq!(
        dec(Format::V5, &hex::decode(want).expect("hex")),
        Ok(reseat(true))
    );
}

/// Catches: a conversion that drops or reorders a field.
#[test]
fn the_command_round_trips_with_its_records_in_order() {
    for stored in [false, true] {
        let command = reseat(stored);
        let bytes = enc(Format::V5, &command).expect("encoded");
        assert_eq!(dec(Format::V5, &bytes), Ok(command));
    }
}

/// Asserts that the encoder writes the command to the same bytes under every format, and
/// that the decoder reads those bytes as the same command under every format, because
/// neither depends on the committed format: a log of any format takes it with no raise.
#[test]
fn every_format_writes_it_and_reads_it() {
    let bytes = enc(Format::V5, &reseat(false)).expect("encoded");
    for n in 1..=5 {
        let format = Format::new(n);
        let wrote = enc(format, &reseat(false));
        assert_eq!(wrote, Ok(bytes.clone()), "{format:?}");
        assert_eq!(dec(format, &bytes), Ok(reseat(false)), "{format:?}");
    }
}

/// Catches: a missing field read as a default.
#[test]
fn a_missing_field_is_refused() {
    let wrap = |c: P| {
        proto::LogCommand {
            command: Some(proto::log_command::Command::Meta(proto::MetaCommand {
                command: Some(c),
            })),
        }
        .encode_to_vec()
    };
    let full = proto::ReseatStaged {
        tier: 2,
        from: Some(proto::ObjectId::default()),
        to: Some(proto::StagedObject::default()),
        stored: false,
    };
    let cases = [
        (
            proto::ReseatStaged {
                from: None,
                ..full.clone()
            },
            "ReseatStaged.from",
        ),
        (
            proto::ReseatStaged {
                to: None,
                ..full.clone()
            },
            "ReseatStaged.to",
        ),
    ];
    for (message, field) in cases {
        let bytes = wrap(P::ReseatStaged(message));
        assert_eq!(dec(Format::V5, &bytes), Err(DecodeError::Missing(field)));
    }
}
