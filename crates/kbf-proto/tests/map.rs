//! `kbf.map.v1` and `kbf_map`: lossless both ways, strict on the way in.
//!
//! Catches:
//! - a state, rule or field lost or swapped in either direction (every state round trips,
//!   a whole map round trips);
//! - an unspecified or unknown enum value, or a replication factor that does not fit a
//!   `u8`, read as a default instead of refused;
//! - a map the domain refuses getting in through the wire;
//! - the generation moving off field 1;
//! - the heartbeat's debts report moving off fields 5 and 6, or an older node's heartbeat
//!   (which sends neither) being read as "owes nothing" instead of "cannot say".

use kbf_map::{DenyEntry, Map, MapError, Member, MemberId, MemberState, Rule};
use kbf_proto::MapDecodeError;
use kbf_proto::kbf::map::v1 as pb;
use prost::Message;

const STATES: [MemberState; 5] = [
    MemberState::Joining,
    MemberState::CatchingUp,
    MemberState::Active,
    MemberState::Draining,
    MemberState::Down,
];

fn member(id: u64, state: MemberState) -> Member {
    Member {
        id: MemberId(id),
        addr: format!("host-{id}:7000"),
        zone: if id.is_multiple_of(2) {
            "a".to_owned()
        } else {
            String::new()
        },
        weight: 1 + id as u32,
        state,
        boot: 0,
    }
}

fn map_of_all_states() -> Map {
    let members = STATES
        .iter()
        .enumerate()
        .map(|(i, s)| member(i as u64 + 1, *s))
        .collect();
    Map::new(
        9,
        members,
        Rule::Rendezvous,
        2,
        4,
        vec![
            DenyEntry::Serial("a1b".to_owned()),
            DenyEntry::Node("mac-07".to_owned()),
        ],
    )
    .unwrap()
}

#[test]
fn every_state_and_the_whole_map_round_trip_through_bytes() {
    let map = map_of_all_states();
    let wire = pb::Map::from(&map);
    for (wire_member, state) in wire.members.iter().zip(STATES) {
        assert_eq!(Member::try_from(wire_member.clone()).unwrap().state, state);
    }
    let decoded = pb::Map::decode(wire.encode_to_vec().as_slice()).unwrap();
    assert_eq!(Map::try_from(decoded).unwrap(), map);
}

#[test]
fn distinct_states_have_distinct_numbers() {
    let numbers: Vec<i32> = STATES
        .iter()
        .map(|s| pb::MemberState::from(*s) as i32)
        .collect();
    assert_eq!(numbers, [1, 2, 3, 4, 5]);
}

#[test]
fn an_unspecified_or_unknown_state_is_refused() {
    for bad in [0, 6, -1, 99] {
        let mut m = pb::Member::from(&member(1, MemberState::Active));
        m.state = bad;
        assert_eq!(Member::try_from(m), Err(MapDecodeError::UnknownState(bad)));
    }
}

#[test]
fn an_unspecified_or_unknown_rule_is_refused() {
    for bad in [0, 2, 77] {
        let mut m = pb::Map::from(&map_of_all_states());
        m.rule = bad;
        assert_eq!(Map::try_from(m), Err(MapDecodeError::UnknownRule(bad)));
    }
}

#[test]
fn a_replication_factor_above_255_is_refused() {
    let mut m = pb::Map::from(&map_of_all_states());
    m.replication = 256;
    assert_eq!(Map::try_from(m), Err(MapDecodeError::Replication(256)));
}

#[test]
fn a_member_with_a_bad_state_refuses_the_map() {
    let mut m = pb::Map::from(&map_of_all_states());
    m.members[2].state = 42;
    assert_eq!(Map::try_from(m), Err(MapDecodeError::UnknownState(42)));
}

#[test]
fn a_map_the_domain_refuses_is_refused_with_the_reason() {
    let mut m = pb::Map::from(&map_of_all_states());
    m.generation = 0;
    let e = Map::try_from(m).unwrap_err();
    assert_eq!(e, MapDecodeError::Invalid(MapError::ZeroGeneration));
    assert_eq!(
        e.to_string(),
        "invalid map: generation 0 is reserved for \"no map seen\""
    );
    assert_eq!(
        MapDecodeError::UnknownState(8).to_string(),
        "unknown member state 8"
    );
    assert_eq!(
        MapDecodeError::UnknownRule(8).to_string(),
        "unknown placement rule 8"
    );
    assert_eq!(
        MapDecodeError::Replication(300).to_string(),
        "replication factor 300 is above 255"
    );
}

// Catches: the generation moved off field 1 (tag 0x08, varint) or the members off field 2.
#[test]
fn the_generation_is_field_one() {
    let wire = pb::Map {
        generation: 5,
        members: vec![pb::Member::default()],
        ..Default::default()
    };
    let bytes = wire.encode_to_vec();
    assert_eq!(&bytes[..2], [0x08, 5]);
    assert_eq!(bytes[2], 0x12, "members are field 2, length-delimited");
}

// Catches: a denied entry that is not read through the one parser, a spelling kept on the
// wire after decoding, a malformed entry accepted, or a repeat that differs only in
// spelling slipping past the duplicate check.
#[test]
fn denied_entries_are_parsed_canonically_and_refused_when_bad_or_repeated() {
    let mut wire = pb::Map::from(&map_of_all_states());
    assert_eq!(
        wire.denied,
        ["serial a1b", "node mac-07"],
        "the canonical form"
    );
    wire.denied = vec!["serial 00:A1:B".to_owned()];
    let map = Map::try_from(wire.clone()).unwrap();
    assert_eq!(pb::Map::from(&map).denied, ["serial a1b"]);

    wire.denied = vec!["serial xyz".to_owned()];
    let e = Map::try_from(wire.clone()).unwrap_err();
    assert!(matches!(e, MapDecodeError::Denied { .. }), "{e}");
    assert_eq!(
        e.to_string(),
        "denied entry \"serial xyz\": a serial is hex digits, colons allowed: \"xyz\""
    );

    wire.denied = vec!["serial a1b".to_owned(), "serial 0A:1B".to_owned()];
    assert_eq!(
        Map::try_from(wire).unwrap_err(),
        MapDecodeError::Invalid(MapError::DuplicateDenied(DenyEntry::Serial(
            "a1b".to_owned()
        )))
    );
}

// Catches: the debts report moved to other field numbers (a node and a frontend of two
// builds would read each other's counts as something else). The bytes are pinned: the
// member id and boot on fields 1 and 2, the count on field 5 and the known flag on 6,
// each a varint.
#[test]
fn the_heartbeat_debts_are_fields_five_and_six() {
    let beat = pb::HeartbeatRequest {
        member_id: 1,
        boot: 2,
        repair_debts: 3,
        debts_known: true,
        ..Default::default()
    };
    assert_eq!(
        beat.encode_to_vec(),
        [0x08, 1, 0x10, 2, 0x28, 3, 0x30, 1],
        "fields 1, 2, 5 and 6"
    );
    let back =
        pb::HeartbeatRequest::decode(&[0x08, 1, 0x10, 2, 0x28, 3, 0x30, 1][..]).expect("decodes");
    assert_eq!(back, beat);
}

// Catches: a heartbeat from a node that predates the report read as having said it owes
// nothing. It carries neither field, so `debts_known` decodes false: "cannot say". And a
// node that says it cannot say sends no bytes for either field.
#[test]
fn a_heartbeat_without_the_report_says_it_cannot_say() {
    let old = pb::HeartbeatRequest::decode(&[0x08, 1, 0x10, 2][..]).expect("decodes");
    assert!(!old.debts_known);
    assert_eq!(old.repair_debts, 0);
    let blind = pb::HeartbeatRequest {
        member_id: 1,
        boot: 2,
        ..Default::default()
    };
    assert_eq!(blind.encode_to_vec(), [0x08, 1, 0x10, 2]);
}

/// Catches: a boot that does not survive the wire, and a reader that fails on a map from a
/// frontend that predates the field. A member's boot round-trips through bytes; a member
/// encoded without field 6 (what an older frontend sends) decodes with boot 0, which
/// means unknown.
#[test]
fn a_members_boot_round_trips_and_an_older_map_without_it_reads_as_zero() {
    let mut m = member(1, MemberState::Active);
    m.boot = 7;
    let wire = pb::Member::from(&m);
    assert_eq!(wire.boot, 7);
    let decoded = pb::Member::decode(wire.encode_to_vec().as_slice()).unwrap();
    assert_eq!(Member::try_from(decoded).unwrap().boot, 7);

    // The bytes an older frontend wrote: the Member as it was, fields 1 to 5 only.
    let old = [
        0x08, 0x01, // id = 1
        0x12, 0x06, b'h', b'o', b's', b't', b'-', b'1', // addr
        0x20, 0x02, // weight = 2
        0x28, 0x03, // state = ACTIVE
    ];
    let decoded = pb::Member::decode(old.as_slice()).unwrap();
    let read = Member::try_from(decoded).unwrap();
    assert_eq!(read.boot, 0, "no field reads as unknown");
    assert_eq!(read.state, MemberState::Active);

    // The field is number 6, a varint: an older reader skips it as an unknown field.
    let with_boot = pb::Member::from(&m).encode_to_vec();
    assert!(
        with_boot.windows(2).any(|w| w == [0x30, 0x07]),
        "field 6 on the wire"
    );
}

// Catches: the cold-store registration fields moved to other numbers (a node and a frontend
// of two builds would read each other's generation as something else), a generation
// that is not a varint, and an apply status the frontend cannot tell from "not sent". The
// bytes are pinned: the answers carry the generation on 5 (Register) and 3 (Heartbeat); the
// heartbeat carries the applied generation on 7 and the status on 8.
#[test]
fn the_cold_store_registration_fields_are_pinned() {
    let register = pb::RegisterResponse {
        objstore_generation: 9,
        ..Default::default()
    };
    assert_eq!(register.encode_to_vec(), [0x28, 9], "field 5");
    let answer = pb::HeartbeatResponse {
        objstore_generation: 9,
        ..Default::default()
    };
    assert_eq!(answer.encode_to_vec(), [0x18, 9], "field 3");
    let beat = pb::HeartbeatRequest {
        applied_objstore_generation: 4,
        apply_status: pb::ApplyStatus::ProbeFailed as i32,
        ..Default::default()
    };
    assert_eq!(beat.encode_to_vec(), [0x38, 4, 0x40, 3], "fields 7 and 8");
}

// Catches: a status enum whose zero is a real outcome (an older node's heartbeat, which
// has no such field, would then read as a report) and an unknown number that decodes
// (the frontend must be able to refuse a status it does not know).
#[test]
fn the_apply_status_is_closed_and_an_older_node_reports_none() {
    assert_eq!(pb::ApplyStatus::Unspecified as i32, 0);
    assert_eq!(pb::ApplyStatus::Ok as i32, 1);
    assert_eq!(pb::ApplyStatus::Pending as i32, 2);
    assert_eq!(pb::ApplyStatus::ProbeFailed as i32, 3);
    assert_eq!(pb::ApplyStatus::IdentityMismatch as i32, 4);
    assert_eq!(pb::ApplyStatus::Unsupported as i32, 5);
    assert!(pb::ApplyStatus::try_from(6).is_err());
    assert!(pb::ApplyStatus::try_from(-1).is_err());
    let old = pb::HeartbeatRequest::decode(&[0x08, 1, 0x10, 2][..]).expect("decodes");
    assert_eq!(old.apply_status, pb::ApplyStatus::Unspecified as i32);
    assert_eq!(old.applied_objstore_generation, 0);
}

// Catches: a node that fails on a Register or Heartbeat answer from a newer frontend (it
// updates first): fields of unknown numbers and wire types after the known ones decode, and
// an objstore generation the node has never seen is a plain number to compare with its own.
#[test]
fn a_newer_frontends_membership_answers_decode() {
    let unknown = [0x78, 0x01, 0x72, 0x02, b'h', b'i'];
    let register = [&[0x28, 0xff, 0xff, 0xff, 0xff, 0x0f][..], &unknown].concat();
    let got = pb::RegisterResponse::decode(register.as_slice()).expect("decodes");
    assert_eq!(got.objstore_generation, u64::from(u32::MAX));
    let beat = [&[0x18, 9][..], &unknown].concat();
    let got = pb::HeartbeatResponse::decode(beat.as_slice()).expect("decodes");
    assert_eq!(got.objstore_generation, 9);
}

// Catches: the open-writes report moved to other field numbers, or colliding with the debts
// (5, 6) or the map generation and catch-up flag (3, 4). The bytes are pinned: the count on
// field 9 and the known flag on 10 (7 and 8 are the cold-store registration report), each a
// varint, beside every field before them.
#[test]
fn the_heartbeat_open_writes_are_fields_seven_and_eight() {
    let beat = pb::HeartbeatRequest {
        member_id: 1,
        boot: 2,
        have_generation: 4,
        catching_up: true,
        repair_debts: 3,
        debts_known: true,
        open_writes: 5,
        writes_known: true,
        ..Default::default()
    };
    let bytes = [
        0x08, 1, 0x10, 2, 0x18, 4, 0x20, 1, 0x28, 3, 0x30, 1, 0x48, 5, 0x50, 1,
    ];
    assert_eq!(beat.encode_to_vec(), bytes, "fields 1 to 6, 9 and 10");
    assert_eq!(
        pb::HeartbeatRequest::decode(&bytes[..]).expect("decodes"),
        beat
    );

    let older = pb::HeartbeatRequest::decode(&bytes[..12]).expect("decodes");
    assert!(
        !older.writes_known,
        "a node that predates the report cannot say"
    );
    assert_eq!(older.open_writes, 0);
}
