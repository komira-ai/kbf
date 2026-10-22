//! A member state and a placement rule this build does not name: the map holds them, and
//! they are inert.

mod common;

use common::{digests, map, member};
use kbf_map::{DenyEntry, Map, Member, MemberId, MemberState, Rule};

/// The states this build names whose placement class is not shared with another.
const NAMED: [MemberState; 4] = [
    MemberState::Joining,
    MemberState::Active,
    MemberState::Draining,
    MemberState::Down,
];

fn two_active() -> Vec<Member> {
    vec![
        member(1, 1, MemberState::Active),
        member(2, 1, MemberState::Active),
    ]
}

/// Two active members and a third in `state`, at replication factor 2.
fn with_third(state: MemberState) -> Map {
    let mut members = two_active();
    members.push(member(3, 1, state));
    map(5, 2, members)
}

fn fp_with(state: MemberState) -> u64 {
    with_third(state).placement_fingerprint()
}

/// Asserts that a member in a state this build does not name is given no work: for every
/// digest it is no write owner and is never read, and the other members own and are read
/// exactly as they are in the map without it. It stays in the map.
#[test]
fn a_member_in_an_unknown_state_is_given_no_work() {
    let with = with_third(MemberState::Unknown(9));
    let without = map(5, 2, two_active());
    for d in &digests(500) {
        assert_eq!(with.write_owners(d), without.write_owners(d));
        assert_eq!(with.read_order(d), without.read_order(d));
    }
    let kept = with.member(MemberId(3)).expect("kept");
    assert_eq!(kept.state, MemberState::Unknown(9));
}

/// Asserts that a member in a state this build does not name takes no writes, whatever the
/// number it came with, while Joining, CatchingUp, Active and Down keep taking writes and
/// Draining keeps taking none.
#[test]
fn an_unknown_state_takes_no_writes_whatever_its_number() {
    for number in [0, 6, 99, -1] {
        let state = MemberState::Unknown(number);
        assert!(!state.takes_writes(), "{number}");
    }
    assert!(MemberState::Active.takes_writes());
    assert!(MemberState::Down.takes_writes());
    assert!(MemberState::Joining.takes_writes());
    assert!(MemberState::CatchingUp.takes_writes());
    assert!(!MemberState::Draining.takes_writes());
}

/// Asserts that a member in a state this build does not name, whatever number it came with
/// and whatever its weight, leaves the placement fingerprint as it is without the member,
/// and that it differs from the fingerprint with the member in each state this build names:
/// the fingerprint says what placement does, and placement ignores such a member.
#[test]
fn an_unknown_state_leaves_the_placement_fingerprint_as_without_the_member() {
    let without = map(5, 2, two_active()).placement_fingerprint();
    assert_eq!(fp_with(MemberState::Unknown(9)), without);
    assert_eq!(fp_with(MemberState::Unknown(-4)), without);
    let mut heavy = two_active();
    heavy.push(member(3, 40, MemberState::Unknown(9)));
    assert_eq!(map(5, 2, heavy).placement_fingerprint(), without);
    for named in NAMED {
        assert_ne!(fp_with(named), without, "{named:?}");
    }
}

/// Asserts that a map under a placement rule this build does not name is read whole, with
/// its members, replication factor, protocol floor and deny list as they came, and that it
/// places no one: no write owner and no read order for any digest.
#[test]
fn a_map_under_an_unknown_rule_places_no_one_and_keeps_the_rest() {
    let denied = vec![DenyEntry::Node("node-x".into())];
    let m = Map::new(5, two_active(), Rule::Unknown(7), 2, 9, denied).expect("a map");
    assert_eq!(m.rule(), Rule::Unknown(7));
    assert_eq!((m.generation(), m.r(), m.min_protocol()), (5, 2, 9));
    assert_eq!(m.members().len(), 2);
    assert_eq!(m.denied(), [DenyEntry::Node("node-x".into())]);
    for d in &digests(200) {
        assert!(m.write_owners(d).is_empty());
        assert!(m.read_order(d).is_empty());
        assert!(m.read_order_with_history(&[], d).is_empty());
    }
}

/// Asserts that the placement fingerprint of a map under an unknown rule is the same for
/// every number the rule came with and for any replication factor and members, because it
/// places no one whatever they are, and that it differs from that of a map under the rule
/// this build names, because the two place differently.
#[test]
fn an_unknown_rule_has_a_placement_fingerprint_of_its_own() {
    let unknown = |number: i32, r: u8, members: Vec<Member>| {
        Map::new(5, members, Rule::Unknown(number), r, 1, Vec::new())
            .expect("a map")
            .placement_fingerprint()
    };
    let alone = unknown(7, 2, two_active());
    assert_eq!(alone, unknown(-3, 2, two_active()));
    assert_eq!(alone, unknown(7, 1, two_active()));
    assert_eq!(
        alone,
        unknown(7, 1, vec![member(9, 3, MemberState::Active)])
    );
    let known = map(5, 2, two_active()).placement_fingerprint();
    assert_ne!(alone, known);
}
