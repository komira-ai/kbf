//! The pack-read counts on the worker `Heartbeat` (field 4, `PackReadCounts`): their numbers and
//! wire types, presence (a daemon that does not read packs sends none, a daemon that does and has
//! counted nothing sends a message of zeros), and a peer on either side of the field.

use kbf_proto::worker;
use prost::Message;

/// A heartbeat of a build before the field: sequence number, report hash and leases only.
#[derive(Clone, PartialEq, Message)]
struct OldHeartbeat {
    #[prost(uint64, tag = "1")]
    seq: u64,
    #[prost(bytes = "vec", tag = "2")]
    report_hash: Vec<u8>,
    #[prost(message, repeated, tag = "3")]
    running: Vec<worker::LeaseId>,
}

/// The heartbeat of an older build that the tests below send.
fn old() -> OldHeartbeat {
    OldHeartbeat {
        seq: 11,
        report_hash: vec![7; 32],
        running: vec![worker::LeaseId { term: 2, seq: 9 }],
    }
}

/// The same heartbeat as a build with the field sends it, carrying `pack_reads`.
fn new(pack_reads: Option<worker::PackReadCounts>) -> worker::Heartbeat {
    worker::Heartbeat {
        seq: 11,
        report_hash: vec![7; 32],
        running: vec![worker::LeaseId { term: 2, seq: 9 }],
        pack_reads,
        pack_writes: None,
        node_load: None,
        input_cache: None,
    }
}

/// The counts with only field `field` (1 to 8) set, to `value`.
fn with(field: u8, value: u64) -> worker::PackReadCounts {
    let mut counts = worker::PackReadCounts::default();
    let slot = match field {
        1 => &mut counts.served,
        2 => &mut counts.served_bytes,
        3 => &mut counts.elsewhere,
        4 => &mut counts.unasked,
        5 => &mut counts.lost,
        6 => &mut counts.damaged,
        7 => &mut counts.fallback_lost,
        8 => &mut counts.fallback_damaged,
        _ => unreachable!("not a pack read count"),
    };
    *slot = value;
    counts
}

/// Asserts that each count is a varint on its own number (served 1, served_bytes 2, elsewhere 3,
/// unasked 4, lost 5, damaged 6, fallback_lost 7, fallback_damaged 8): a count moved to another
/// number or made another type would be read as a different count by a frontend of another build.
#[test]
fn each_count_is_a_varint_on_its_number() {
    for field in 1_u8..=8 {
        let counts = with(field, 9);
        let want = [field << 3, 9];
        assert_eq!(counts.encode_to_vec(), want, "field {field}");
        let back = worker::PackReadCounts::decode(&want[..]).expect("decodes");
        assert_eq!(back, counts, "field {field}");
    }
}

/// Asserts that the counts ride the heartbeat as field 4, length-delimited, after the leases, and
/// that a heartbeat with them decodes to what was sent.
#[test]
fn the_counts_are_field_four_of_the_heartbeat() {
    let counts = worker::PackReadCounts {
        served: 1,
        served_bytes: 2,
        ..worker::PackReadCounts::default()
    };
    let beat = new(Some(counts));
    let mut want = old().encode_to_vec();
    want.extend_from_slice(&[0x22, 4, 0x08, 1, 0x10, 2]);
    assert_eq!(beat.encode_to_vec(), want);
    let back = worker::Heartbeat::decode(&want[..]).expect("decodes");
    assert_eq!(back, beat);
}

/// Asserts that a heartbeat without the counts is the bytes a build before the field sent: the
/// heartbeat of a daemon that does not read packs is unchanged.
#[test]
fn a_heartbeat_without_the_counts_is_the_bytes_it_was_before() {
    assert_eq!(new(None).encode_to_vec(), old().encode_to_vec());
}

/// Asserts that a message of zeros is on the wire and decodes as present, where no message
/// decodes as absent: a daemon that reads packs and has counted nothing yet is told from a
/// daemon that does not read packs.
#[test]
fn zeros_are_present_and_nothing_is_absent() {
    let zeros = new(Some(worker::PackReadCounts::default()));
    let mut want = old().encode_to_vec();
    want.extend_from_slice(&[0x22, 0]);
    assert_eq!(zeros.encode_to_vec(), want);
    let back = worker::Heartbeat::decode(&want[..]).expect("decodes");
    assert_eq!(back.pack_reads, Some(worker::PackReadCounts::default()));
    let silent = worker::Heartbeat::decode(&[0x08, 1][..]).expect("decodes");
    assert_eq!(silent.pack_reads, None);
}

/// Asserts that a frontend of a build before the field decodes a heartbeat that carries the
/// counts, skipping them, and reads the rest as sent.
#[test]
fn a_frontend_before_the_field_skips_it() {
    let counts = with(1, 5);
    let beat = new(Some(counts));
    let seen = OldHeartbeat::decode(&beat.encode_to_vec()[..]).expect("an older peer decodes");
    assert_eq!(seen, old());
}

/// Asserts that a frontend of this build decodes the heartbeat of a daemon before the field as
/// one with no counts, the rest as sent.
#[test]
fn a_heartbeat_before_the_field_decodes_with_no_counts() {
    let seen = worker::Heartbeat::decode(&old().encode_to_vec()[..]).expect("decodes");
    assert_eq!(seen, new(None));
}

/// The pack-read counts of a build before `fallback_lost` and `fallback_damaged` (fields 7 and 8).
#[derive(Clone, PartialEq, Message)]
struct SixCounts {
    #[prost(uint64, tag = "1")]
    served: u64,
    #[prost(uint64, tag = "2")]
    served_bytes: u64,
    #[prost(uint64, tag = "3")]
    elsewhere: u64,
    #[prost(uint64, tag = "4")]
    unasked: u64,
    #[prost(uint64, tag = "5")]
    lost: u64,
    #[prost(uint64, tag = "6")]
    damaged: u64,
}

/// Asserts that a frontend of the previous release, which knows six counts, reads the counts of a
/// daemon that fell back from its tierstore as no failed read: the two new counts are skipped and
/// `lost` and `damaged` stay 0, the six it knows read as sent.
/// Catches a reader of the previous release that would count reads that succeeded as failed.
#[test]
fn a_frontend_of_the_previous_release_skips_the_fallback_counts() {
    let counts = worker::PackReadCounts {
        served: 3,
        served_bytes: 30,
        elsewhere: 1,
        unasked: 2,
        lost: 0,
        damaged: 0,
        fallback_lost: 4,
        fallback_damaged: 5,
    };
    let seen = SixCounts::decode(&counts.encode_to_vec()[..]).expect("an older peer decodes");
    let want = SixCounts {
        served: 3,
        served_bytes: 30,
        elsewhere: 1,
        unasked: 2,
        lost: 0,
        damaged: 0,
    };
    assert_eq!(seen, want);
}

/// Asserts that a frontend of this build reads the counts of a daemon of the previous release as
/// sent, with both fallback counts 0.
#[test]
fn counts_of_the_previous_release_decode_with_no_fallbacks() {
    let old = SixCounts {
        served: 1,
        served_bytes: 2,
        elsewhere: 3,
        unasked: 4,
        lost: 5,
        damaged: 6,
    };
    let seen = worker::PackReadCounts::decode(&old.encode_to_vec()[..]).expect("decodes");
    let want = worker::PackReadCounts {
        served: 1,
        served_bytes: 2,
        elsewhere: 3,
        unasked: 4,
        lost: 5,
        damaged: 6,
        fallback_lost: 0,
        fallback_damaged: 0,
    };
    assert_eq!(seen, want);
}
