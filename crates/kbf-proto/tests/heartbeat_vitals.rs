//! The write-tier health on the storage heartbeat (`HeartbeatRequest` fields 17 to 23):
//! their numbers and wire types, presence (an unset field is "unknown", a set 0 is a
//! real zero), and a peer that predates them.
//!
//! Catches: a field moved to another number or made another type (a node and a frontend
//! of two builds would read each other's values as something else); presence lost (a node
//! that sends nothing would read as having said 0); and a peer that fails on the fields.

use kbf_proto::kbf::map::v1 as pb;
use prost::Message;

/// A heartbeat of an older build: members and boot only, as the field set stood before.
#[derive(Clone, PartialEq, Message)]
struct OldHeartbeat {
    #[prost(uint64, tag = "1")]
    member_id: u64,
    #[prost(uint64, tag = "2")]
    boot: u64,
}

/// A heartbeat with only field `field` (17 to 23) set, to `value`.
fn with(field: usize, value: u64) -> pb::HeartbeatRequest {
    let mut beat = pb::HeartbeatRequest::default();
    let slot = match field {
        17 => &mut beat.unflushed_bytes,
        18 => &mut beat.unflushed_blocks,
        19 => &mut beat.oldest_unflushed_ms,
        20 => &mut beat.budget_bytes,
        21 => &mut beat.used_bytes,
        22 => &mut beat.flush_failures,
        23 => &mut beat.flush_conflicts,
        _ => unreachable!("not a storage-health field"),
    };
    *slot = Some(value);
    beat
}

#[test]
fn each_field_is_a_varint_on_its_number() {
    for field in 17_u8..=23 {
        // The tag of a varint field of number 17 or more takes two bytes: (n << 3) in
        // seven-bit groups, the low group first with the continuation bit set.
        let tag = u32::from(field) << 3;
        let want = [
            u8::try_from(tag & 0x7f).expect("seven bits") | 0x80,
            u8::try_from(tag >> 7).expect("one more group"),
            9,
        ];
        let beat = with(usize::from(field), 9);
        assert_eq!(beat.encode_to_vec(), want, "field {field}");
        assert_eq!(
            pb::HeartbeatRequest::decode(&want[..]).expect("decodes"),
            beat
        );
    }
    // The first and the last, spelled out.
    assert_eq!(with(17, 9).encode_to_vec(), [0x88, 0x01, 9]);
    assert_eq!(with(23, 9).encode_to_vec(), [0xb8, 0x01, 9]);
}

#[test]
fn a_set_zero_is_on_the_wire_and_an_unset_field_is_not() {
    assert_eq!(
        pb::HeartbeatRequest::default().encode_to_vec(),
        Vec::<u8>::new()
    );
    assert_eq!(with(17, 0).encode_to_vec(), [0x88, 0x01, 0]);
    let zero = pb::HeartbeatRequest::decode(&[0x88, 0x01, 0][..]).expect("decodes");
    assert_eq!(zero.unflushed_bytes, Some(0), "a real zero");
    let silent = pb::HeartbeatRequest::decode(&[0x08, 1, 0x10, 2][..]).expect("decodes");
    for value in [
        silent.unflushed_bytes,
        silent.unflushed_blocks,
        silent.oldest_unflushed_ms,
        silent.budget_bytes,
        silent.used_bytes,
        silent.flush_failures,
        silent.flush_conflicts,
    ] {
        assert_eq!(value, None, "a peer that predates them says nothing");
    }
}

#[test]
fn a_peer_that_predates_the_fields_decodes_a_beat_that_has_them() {
    let beat = pb::HeartbeatRequest {
        member_id: 1,
        boot: 2,
        unflushed_bytes: Some(3),
        flush_conflicts: Some(4),
        ..Default::default()
    };
    let old = OldHeartbeat::decode(&beat.encode_to_vec()[..]).expect("an older peer decodes");
    assert_eq!((old.member_id, old.boot), (1, 2));
}
