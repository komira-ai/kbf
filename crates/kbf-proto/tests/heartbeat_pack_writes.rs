//! The pack-write counts on the worker `Heartbeat` (field 5, `PackWriteCounts`): their numbers
//! and wire types, presence (a daemon that does not write packs sends none, a daemon that does
//! and has counted nothing sends a message of zeros), and a peer on either side of the field.

use kbf_proto::worker;
use prost::Message;

/// A heartbeat of a build before the field: the fields up to the pack-read counts.
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
}

/// The heartbeat of an older build that the tests below send.
fn old() -> OldHeartbeat {
    OldHeartbeat {
        seq: 11,
        report_hash: vec![7; 32],
        running: vec![worker::LeaseId { term: 2, seq: 9 }],
        pack_reads: Some(worker::PackReadCounts {
            served: 1,
            ..worker::PackReadCounts::default()
        }),
    }
}

/// The same heartbeat as a build with the field sends it, carrying `pack_writes`.
fn new(pack_writes: Option<worker::PackWriteCounts>) -> worker::Heartbeat {
    worker::Heartbeat {
        seq: 11,
        report_hash: vec![7; 32],
        running: vec![worker::LeaseId { term: 2, seq: 9 }],
        pack_reads: old().pack_reads,
        pack_writes,
        node_load: None,
        input_cache: None,
    }
}

/// The counts with only field `field` (1 to 12) set, to `value`.
fn with(field: u8, value: u64) -> worker::PackWriteCounts {
    let mut counts = worker::PackWriteCounts::default();
    let slot = match field {
        1 => &mut counts.packs,
        2 => &mut counts.packed,
        3 => &mut counts.packed_bytes,
        4 => &mut counts.fallback_off,
        5 => &mut counts.fallback_unfit,
        6 => &mut counts.fallback_unsupported,
        7 => &mut counts.fallback_refused,
        8 => &mut counts.fallback_store,
        9 => &mut counts.fallback_record,
        10 => &mut counts.fallback_index,
        11 => &mut counts.missing_indexed,
        12 => &mut counts.missing_unasked,
        _ => unreachable!("not a pack write count"),
    };
    *slot = value;
    counts
}

/// Asserts that each count is a varint on its own number (packs 1, packed 2, packed_bytes 3, the
/// seven fallbacks 4 to 10, missing_indexed 11, missing_unasked 12): a count moved to another
/// number or made another type would be read as a different count by a frontend of another
/// build.
#[test]
fn each_count_is_a_varint_on_its_number() {
    for field in 1_u8..=12 {
        let counts = with(field, 9);
        let want = [field << 3, 9];
        assert_eq!(counts.encode_to_vec(), want, "field {field}");
        let back = worker::PackWriteCounts::decode(&want[..]).expect("decodes");
        assert_eq!(back, counts, "field {field}");
    }
}

/// Asserts that the counts ride the heartbeat as field 5, length-delimited, after the pack-read
/// counts, and that a heartbeat with them decodes to what was sent.
#[test]
fn the_counts_are_field_five_of_the_heartbeat() {
    let counts = worker::PackWriteCounts {
        packs: 1,
        packed: 2,
        ..worker::PackWriteCounts::default()
    };
    let beat = new(Some(counts));
    let mut want = old().encode_to_vec();
    want.extend_from_slice(&[0x2a, 4, 0x08, 1, 0x10, 2]);
    assert_eq!(beat.encode_to_vec(), want);
    let back = worker::Heartbeat::decode(&want[..]).expect("decodes");
    assert_eq!(back, beat);
}

/// Asserts that a heartbeat without the counts is the bytes a build before the field sent: the
/// heartbeat of a daemon that does not write packs is unchanged.
#[test]
fn a_heartbeat_without_the_counts_is_the_bytes_it_was_before() {
    assert_eq!(new(None).encode_to_vec(), old().encode_to_vec());
}

/// Asserts that a message of zeros is on the wire and decodes as present, where no message
/// decodes as absent: a daemon that writes packs and has counted nothing yet is told from a
/// daemon that does not write packs.
#[test]
fn zeros_are_present_and_nothing_is_absent() {
    let zeros = new(Some(worker::PackWriteCounts::default()));
    let mut want = old().encode_to_vec();
    want.extend_from_slice(&[0x2a, 0]);
    assert_eq!(zeros.encode_to_vec(), want);
    let back = worker::Heartbeat::decode(&want[..]).expect("decodes");
    assert_eq!(back.pack_writes, Some(worker::PackWriteCounts::default()));
    let silent = worker::Heartbeat::decode(&[0x08, 1][..]).expect("decodes");
    assert_eq!(silent.pack_writes, None);
}

/// Asserts that a frontend of a build before the field decodes a heartbeat that carries the
/// counts, skipping them, and reads the rest as sent.
#[test]
fn a_frontend_before_the_field_skips_it() {
    let beat = new(Some(with(1, 5)));
    let seen = OldHeartbeat::decode(&beat.encode_to_vec()[..]).expect("an older peer decodes");
    assert_eq!(seen, old());
}

/// Asserts that a frontend of this build decodes the heartbeat of a daemon before the field as
/// one with no write counts, the rest as sent.
#[test]
fn a_heartbeat_before_the_field_decodes_with_no_counts() {
    let seen = worker::Heartbeat::decode(&old().encode_to_vec()[..]).expect("decodes");
    assert_eq!(seen, new(None));
}
