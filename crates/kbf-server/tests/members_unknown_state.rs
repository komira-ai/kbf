//! A member row whose state this build does not name (`kbf_server::members`): a later build
//! wrote it. The row reads, the member stays in the map but is given no work and is not
//! counted as active, a heartbeat keeps the number it had, and a frontend that rebuilds the
//! map keeps the placement rule it read. Every test runs on the fake store with its
//! injectable clock.

#[path = "support/hooked.rs"]
mod hooked;

use std::sync::Arc;
use std::time::Duration;

use hooked::Hooked;
use kbf_map::{Map, MemberId, MemberState, Rule};
use kbf_proto::kbf::map::v1 as pb;
use kbf_server::members::{
    Members, MembershipConfig, MembershipError, PROTOCOL, Progress, Registrant,
};
use kbf_server::storage_health::Reported;
use kbf_state::{Cond, Store, Txn};
use kbf_types::{Digest, DigestFunction};
use prost::Message as _;
use sha2::{Digest as _, Sha256};

/// A row's member and nothing else: appended to a stored row, it overrides the member's
/// fields that it sets (a protobuf message that is decoded twice is merged), which is how
/// a test writes "the state of this row" without knowing the rest of its encoding.
#[derive(Clone, PartialEq, prost::Message)]
struct OnlyMember {
    #[prost(message, optional, tag = "1")]
    member: Option<pb::Member>,
}

fn members(store: &Arc<Hooked>) -> Members {
    let config = MembershipConfig {
        ttl: Duration::from_secs(3600),
        dwell: Duration::from_secs(7200),
        replication: 2,
    };
    Members::new(Arc::clone(store) as Arc<dyn Store>, config)
}

fn node(i: u32) -> Registrant {
    Registrant {
        instance_id: format!("node-{i}"),
        addr: format!("192.0.2.{i}:8982"),
        zone: String::new(),
        weight: 1,
        protocol: PROTOCOL,
        progress: Progress::CaughtUp,
    }
}

fn digests() -> Vec<Digest> {
    (0_u64..200)
        .map(|i| {
            let hash: [u8; 32] = Sha256::digest(i.to_be_bytes()).into();
            Digest::new(DigestFunction::Sha256, hash, 1000 + i)
        })
        .collect()
}

fn state_of(map: &Map, id: MemberId) -> Option<MemberState> {
    map.member(id).map(|m| m.state)
}

/// Makes the stored row of member `id` say state `number`, as a later build would write it.
fn write_state(store: &Hooked, id: MemberId, number: i32) {
    let key = format!("member/{}", id.0);
    let mut value = store.fake.get(&key).expect("read").expect("a row").value;
    let later = OnlyMember {
        member: Some(pb::Member {
            state: number,
            ..pb::Member::default()
        }),
    };
    value.extend(later.encode_to_vec());
    store
        .fake
        .txn(Txn::new().put(&key, value, Cond::Any))
        .expect("store")
        .expect("commit");
}

/// Makes the stored map say placement rule `number`, as a later build would write it.
fn write_rule(store: &Hooked, number: i32) {
    let value = store.fake.get("map").expect("read").expect("a map").value;
    let mut wire = pb::Map::decode(value.as_slice()).expect("a map");
    wire.rule = number;
    store
        .fake
        .txn(Txn::new().put("map", wire.encode_to_vec(), Cond::Any))
        .expect("store")
        .expect("commit");
}

/// The state number the stored row of member `id` holds.
fn stored_state(store: &Hooked, id: MemberId) -> i32 {
    let key = format!("member/{}", id.0);
    let value = store.fake.get(&key).expect("read").expect("a row").value;
    let row = OnlyMember::decode(value.as_slice()).expect("a row");
    row.member.expect("a member").state
}

/// Asserts that a stored member row whose state this build does not name reads: the map is
/// rebuilt with the member in an unknown state that keeps its number, instead of the read
/// failing as a corrupt row, and for every digest the member is neither a write owner nor
/// read from while the other member owns and is read as before. The rows that other calls
/// list read as well.
#[test]
fn a_stored_row_in_an_unknown_state_reads_and_the_member_gets_no_work() {
    let s = Hooked::new();
    let m = members(&s);
    let a = m.register(&node(1)).expect("a");
    let b = m.register(&node(2)).expect("b");
    write_state(&s, b.id, 9);

    let map = m.refresh().expect("refresh").expect("a map");
    assert_eq!(state_of(&map, a.id), Some(MemberState::Active));
    assert_eq!(state_of(&map, b.id), Some(MemberState::Unknown(9)));
    for d in &digests() {
        assert_eq!(map.write_owners(d), [a.id]);
        assert_eq!(map.read_order(d), [a.id]);
    }
    assert!(m.protocols().expect("rows read").contains_key(&b.id));
    assert!(m.node_facts().expect("rows read").contains_key(&b.id));
}

/// Asserts that a member whose row holds a state this build does not name is not counted as
/// active: the judgement over the members counts it among those that are not.
#[test]
fn a_member_in_an_unknown_state_is_not_counted_as_active() {
    let s = Hooked::new();
    let m = members(&s);
    m.register(&node(1)).expect("a");
    let b = m.register(&node(2)).expect("b");
    write_state(&s, b.id, 9);

    let health = m.storage_health().expect("health");
    assert_eq!(health.members_total, 2);
    assert_eq!(health.members_active, 1);
    assert_eq!(health.not_active, [b.id]);
}

/// Asserts that a heartbeat of a member whose row holds a state this build does not name
/// neither promotes it nor changes it, and that the row it rewrites says the number it came
/// with, so the build that wrote the state reads its own state back.
#[test]
fn a_heartbeat_keeps_the_state_a_later_build_wrote() {
    let s = Hooked::new();
    let m = members(&s);
    m.register(&node(1)).expect("a");
    let b = m.register(&node(2)).expect("b");
    write_state(&s, b.id, 9);

    m.heartbeat(b.id, b.boot, Progress::CaughtUp, Reported::default(), None)
        .expect("beat");
    assert_eq!(stored_state(&s, b.id), 9);
    let map = m.refresh().expect("refresh").expect("a map");
    assert_eq!(state_of(&map, b.id), Some(MemberState::Unknown(9)));
}

/// Asserts that a frontend that rebuilds the map after a change to a member keeps the
/// placement rule it read, a rule this build does not name, and does not replace it with the
/// one it knows: the generation goes up and the member changes, and the stored map says the
/// rule it said.
#[test]
fn a_rebuilt_map_keeps_the_placement_rule_it_read() {
    let s = Hooked::new();
    let m = members(&s);
    m.register(&node(1)).expect("a");
    let b = m.register(&node(2)).expect("b");
    write_rule(&s, 2);
    let before = m.current_map().expect("read").expect("a map").generation();
    write_state(&s, b.id, 9);

    let map = m.refresh().expect("refresh").expect("a map");
    assert_eq!(map.generation(), before + 1);
    assert_eq!(state_of(&map, b.id), Some(MemberState::Unknown(9)));
    assert_eq!(map.rule(), Rule::Unknown(2));
    let value = s.fake.get("map").expect("read").expect("a map").value;
    assert_eq!(pb::Map::decode(value.as_slice()).expect("a map").rule, 2);
}

/// Asserts that a state this build does not name cannot be set by hand: the call is refused
/// as not settable and the member's row is left as it was.
#[test]
fn a_state_this_build_does_not_name_cannot_be_set() {
    let s = Hooked::new();
    let m = members(&s);
    let a = m.register(&node(1)).expect("a");
    let result = m.set_state(a.id, MemberState::Unknown(9));
    assert!(matches!(
        result,
        Err(MembershipError::NotSettable(MemberState::Unknown(9)))
    ));
    let active = i32::from(pb::MemberState::Active);
    assert_eq!(stored_state(&s, a.id), active);
}
