//! The `kbf.log.v1` encoding of a log's `Bootstrap` command.
//!
//! Catches:
//! - a `Bootstrap` encoding that changes under a committed log (a tag swapped, the
//!   command moved to another `oneof` number): the golden bytes below are format 1's;
//! - a conversion that loses or mixes up the log id or the prefix (a round trip);
//! - a decoder that reads an absent or zero log id as a log id, which the daemon would
//!   later take for "no epoch named";
//! - a log id written in another form than the 16 hex digits object keys use.

use std::num::NonZeroU64;

use kbf_log::{Bootstrap, DecodeError, FarmLogId, Format, LogCommand, decode, encode};

fn id(n: u64) -> FarmLogId {
    FarmLogId::new(NonZeroU64::new(n).expect("non-zero"))
}

fn bootstrap(log_id: u64, store_prefix: &str) -> LogCommand {
    LogCommand::Bootstrap(Bootstrap {
        log_id: id(log_id),
        store_prefix: store_prefix.to_owned(),
    })
}

/// Written out from `log.proto`: `LogCommand` field 2 (`12`, 33 bytes), around
/// `Bootstrap { log_id (08) 0x0123456789abcdef as a 9-byte varint, store_prefix (12)
/// 21 bytes }`.
#[test]
fn a_bootstrap_has_its_golden_bytes() {
    let command = bootstrap(0x0123_4567_89ab_cdef, "kbf/0123456789abcdef/");
    let golden = concat!(
        "1221",
        "08",
        "ef9bafcdf8acd19101",
        "1215",
        "6b62662f303132333435363738396162636465662f"
    );
    let bytes = encode(Format::V1, &command).expect("encode");
    assert_eq!(hex::encode(&bytes), golden);
    assert_eq!(decode(Format::V1, &bytes), Ok(command));
}

#[test]
fn bootstraps_round_trip() {
    for command in [
        bootstrap(1, ""),
        bootstrap(u64::MAX, "kbf/ffffffffffffffff/"),
        bootstrap(42, "a/b/"),
    ] {
        let bytes = encode(Format::V1, &command).expect("encode");
        assert_eq!(decode(Format::V1, &bytes), Ok(command));
    }
}

/// A zero log id is the proto3 default, so an absent one and an explicit zero both
/// read as zero; both are refused.
#[test]
fn a_bootstrap_without_a_log_id_is_refused() {
    for hex in ["1200", "12020800", "1203120161"] {
        let bytes = hex::decode(hex).expect("hex");
        assert_eq!(
            decode(Format::V1, &bytes),
            Err(DecodeError::Missing("Bootstrap.log_id")),
            "{hex}"
        );
    }
}

#[test]
fn a_log_id_displays_as_16_hex_digits() {
    assert_eq!(id(1).to_string(), "0000000000000001");
    assert_eq!(id(0xabc).to_string(), "0000000000000abc");
    assert_eq!(id(u64::MAX).to_string(), "ffffffffffffffff");
    assert_eq!(id(7).get(), 7);
}
