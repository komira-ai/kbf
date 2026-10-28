//! The busy rate on the worker `Heartbeat` (field 6, `optional uint64 busy_millis`): its
//! number and wire type, presence (a daemon that does not measure sends none, a daemon
//! that measures an idle node sends 0), and a peer on either side of the field.

use kbf_proto::worker;
use prost::Message;

/// A heartbeat of a build before the field: the fields up to the pack-write counts.
#[derive(Clone, PartialEq, Message)]
struct OldHeartbeat {
    #[prost(uint64, tag = "1")]
    seq: u64,
    #[prost(bytes = "vec", tag = "2")]
    report_hash: Vec<u8>,
    #[prost(message, repeated, tag = "3")]
    running: Vec<worker::LeaseId>,
    #[prost(message, optional, tag = "4")]
    pack_reads: Option<worker::PackReadCounts>,
    #[prost(message, optional, tag = "5")]
    pack_writes: Option<worker::PackWriteCounts>,
}

fn old() -> OldHeartbeat {
    OldHeartbeat {
        seq: 11,
        report_hash: vec![7; 32],
        running: vec![worker::LeaseId { term: 2, seq: 9 }],
        pack_reads: None,
        pack_writes: None,
    }
}

/// The same heartbeat as a build with the field sends it, carrying `busy_millis`.
fn new(busy_millis: Option<u64>) -> worker::Heartbeat {
    worker::Heartbeat {
        seq: 11,
        report_hash: vec![7; 32],
        running: vec![worker::LeaseId { term: 2, seq: 9 }],
        pack_reads: None,
        pack_writes: None,
        busy_millis,
    }
}

/// Asserts that the rate rides the heartbeat as a varint on field 6 and decodes to what was
/// sent. Catches a rate moved to another number or made another type, which a server of
/// another build would read as a different field.
#[test]
fn the_rate_is_a_varint_on_field_six() {
    let beat = new(Some(300));
    let mut want = old().encode_to_vec();
    want.extend_from_slice(&[0x30, 0xac, 0x02]);
    assert_eq!(beat.encode_to_vec(), want);
    let back = worker::Heartbeat::decode(&want[..]).expect("decodes");
    assert_eq!(back, beat);
}

/// Asserts that a heartbeat without a rate is the bytes a build before the field sent, and
/// that 0 is on the wire and decodes as present: a daemon that measures an idle node is told
/// from one that does not measure, which the scheduler reads as unknown.
#[test]
fn zero_is_present_and_nothing_is_absent() {
    assert_eq!(new(None).encode_to_vec(), old().encode_to_vec());
    let mut zero = old().encode_to_vec();
    zero.extend_from_slice(&[0x30, 0]);
    assert_eq!(new(Some(0)).encode_to_vec(), zero);
    let back = worker::Heartbeat::decode(&zero[..]).expect("decodes");
    assert_eq!(back.busy_millis, Some(0));
    let silent = worker::Heartbeat::decode(&[0x08, 1][..]).expect("decodes");
    assert_eq!(silent.busy_millis, None);
}

/// Asserts that a server of a build before the field decodes a heartbeat that carries the
/// rate, skipping it, and reads the rest as sent; and that a server of this build reads the
/// heartbeat of an older daemon as one with no rate.
#[test]
fn a_peer_on_either_side_of_the_field_reads_the_rest() {
    let seen = OldHeartbeat::decode(&new(Some(300)).encode_to_vec()[..]).expect("older decodes");
    assert_eq!(seen, old());
    let seen = worker::Heartbeat::decode(&old().encode_to_vec()[..]).expect("decodes");
    assert_eq!(seen, new(None));
}
