//! The input cache counts on the worker `Heartbeat` (field 7, `InputCacheCounts`): their numbers
//! and wire types, presence (a daemon with no cache sends none, a daemon whose cache has done
//! nothing sends a message of zeros), and a peer on either side of the field.

use kbf_proto::worker;
use prost::Message;

/// A heartbeat of a build before the field: the fields up to the node load.
#[derive(Clone, PartialEq, Message)]
struct OldHeartbeat {
    #[prost(uint64, tag = "1")]
    seq: u64,
    #[prost(bytes = "vec", tag = "2")]
    report_hash: Vec<u8>,
    #[prost(message, optional, tag = "6")]
    node_load: Option<worker::NodeLoad>,
}

fn old() -> OldHeartbeat {
    OldHeartbeat {
        seq: 3,
        report_hash: vec![5; 32],
        node_load: None,
    }
}

/// The same heartbeat as a build with the field sends it, carrying `input_cache`.
fn new(input_cache: Option<worker::InputCacheCounts>) -> worker::Heartbeat {
    worker::Heartbeat {
        seq: 3,
        report_hash: vec![5; 32],
        input_cache,
        ..worker::Heartbeat::default()
    }
}

/// The counts with only field `field` (1 to 6) set, to `value`.
fn with(field: u8, value: u64) -> worker::InputCacheCounts {
    let mut counts = worker::InputCacheCounts::default();
    let slot = match field {
        1 => &mut counts.fills,
        2 => &mut counts.fill_bytes,
        3 => &mut counts.hits,
        4 => &mut counts.hit_bytes,
        5 => &mut counts.evictions,
        6 => &mut counts.evicted_bytes,
        _ => unreachable!("not an input cache count"),
    };
    *slot = value;
    counts
}

/// Asserts that each count is a varint on its own number (fills 1, fill_bytes 2, hits 3,
/// hit_bytes 4, evictions 5, evicted_bytes 6): a count moved to another number or made another
/// type would be read as a different count by a frontend of another build.
#[test]
fn each_count_is_a_varint_on_its_number() {
    for field in 1_u8..=6 {
        let counts = with(field, 9);
        let want = [field << 3, 9];
        assert_eq!(counts.encode_to_vec(), want, "field {field}");
        let back = worker::InputCacheCounts::decode(&want[..]).expect("decodes");
        assert_eq!(back, counts, "field {field}");
    }
}

/// Asserts that the tree counts are field 7 of the counts, length-delimited, with builds 1, hits 2
/// and evictions 3.
#[test]
fn the_tree_counts_are_field_seven_and_their_own_numbers() {
    let counts = worker::InputCacheCounts {
        trees: Some(worker::TreeCounts {
            builds: 1,
            hits: 2,
            evictions: 3,
        }),
        ..worker::InputCacheCounts::default()
    };
    let want = [0x3a, 6, 0x08, 1, 0x10, 2, 0x18, 3];
    assert_eq!(counts.encode_to_vec(), want);
    let back = worker::InputCacheCounts::decode(&want[..]).expect("decodes");
    assert_eq!(back, counts);
}

/// Asserts that the counts ride the heartbeat as field 7, length-delimited, after the node load
/// (6), and that a heartbeat with them decodes to what was sent.
#[test]
fn the_counts_are_field_seven_of_the_heartbeat() {
    let beat = new(Some(with(1, 4)));
    let mut want = old().encode_to_vec();
    want.extend_from_slice(&[0x3a, 2, 0x08, 4]);
    assert_eq!(beat.encode_to_vec(), want);
    let back = worker::Heartbeat::decode(&want[..]).expect("decodes");
    assert_eq!(back, beat);
}

/// Asserts that a heartbeat without the counts is the bytes a build before the field sent, and
/// that a build before the field reads a heartbeat with them (it skips the unknown field).
#[test]
fn a_peer_on_either_side_of_the_field_is_understood() {
    assert_eq!(new(None).encode_to_vec(), old().encode_to_vec());
    let sent = new(Some(with(3, 7))).encode_to_vec();
    let read = OldHeartbeat::decode(&sent[..]).expect("an old build skips the field");
    assert_eq!(read, old());
}

/// Asserts that a message of zeros is on the wire and decodes as present, where no message
/// decodes as absent: a cache that has done nothing yet is told from no cache.
#[test]
fn zeros_are_present_and_nothing_is_absent() {
    let zeros = new(Some(worker::InputCacheCounts::default()));
    let mut want = old().encode_to_vec();
    want.extend_from_slice(&[0x3a, 0]);
    assert_eq!(zeros.encode_to_vec(), want);
    let back = worker::Heartbeat::decode(&want[..]).expect("decodes");
    assert_eq!(back.input_cache, Some(worker::InputCacheCounts::default()));
    let silent = worker::Heartbeat::decode(&[0x08, 1][..]).expect("decodes");
    assert_eq!(silent.input_cache, None);
}
