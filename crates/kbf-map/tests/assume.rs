//! `Map::owners_assuming`: what a member would own if it were in another state.

mod common;

use common::{digests, ids, map, member};
use kbf_map::{MemberId, MemberState};

/// The map `base` with member `id` set to `state`, built by hand (not through `assuming`).
fn with_state(r: u8, members: &[(u64, MemberState)], id: u64, state: MemberState) -> kbf_map::Map {
    let members = members
        .iter()
        .map(|&(m, s)| member(m, 1, if m == id { state } else { s }))
        .collect();
    map(2, r, members)
}

const R1_FARM: [(u64, MemberState); 4] = [
    (1, MemberState::Active),
    (2, MemberState::Active),
    (3, MemberState::Active),
    (4, MemberState::Joining),
];

const R3_FARM: [(u64, MemberState); 5] = [
    (1, MemberState::Active),
    (2, MemberState::Active),
    (3, MemberState::Active),
    (4, MemberState::Active),
    (5, MemberState::Joining),
];

fn current(r: u8, farm: &[(u64, MemberState)]) -> kbf_map::Map {
    map(2, r, farm.iter().map(|&(m, s)| member(m, 1, s)).collect())
}

// Catches: an assumption that is not the owner list of the map where the member really is
// Active (a different tie-break, a joiner still demoted), at r = 1.
#[test]
fn a_joiner_assumed_active_owns_what_an_active_member_would_at_r1() {
    let now = current(1, &R1_FARM);
    let promoted = with_state(1, &R1_FARM, 4, MemberState::Active);
    let mut owned = 0usize;
    let all = digests(4000);
    for d in &all {
        let got = now
            .owners_assuming(MemberId(4), MemberState::Active, d)
            .expect("member 4 exists");
        assert_eq!(got, promoted.write_owners(d));
        if got == ids(&[4]) {
            owned += 1;
        }
    }
    // About 1/4 of the digests (weights are equal).
    assert!((600..=1400).contains(&owned), "owned {owned} of 4000");
}

// Catches: a joiner that owns something at r = 1 today (it must not), and an assumption
// that changes nothing.
#[test]
fn a_joining_member_is_never_rank_one_and_owns_nothing_at_r1() {
    let now = current(1, &R1_FARM);
    for d in &digests(2000) {
        assert!(!now.write_owners(d).contains(&MemberId(4)));
        let same = now
            .owners_assuming(MemberId(4), MemberState::Joining, d)
            .expect("member 4 exists");
        assert_eq!(same, now.write_owners(d));
    }
}

// Catches: the r = 3 case: a joiner assumed Active takes about r/N of the digests and
// matches the hand-built map; as Joining it is never the first of the list.
#[test]
fn a_joiner_assumed_active_owns_r_over_n_of_digests_at_r3() {
    let now = current(3, &R3_FARM);
    let promoted = with_state(3, &R3_FARM, 5, MemberState::Active);
    let mut owned = 0usize;
    let mut first = 0usize;
    let all = digests(4000);
    for d in &all {
        assert_ne!(
            now.write_owners(d).first(),
            Some(&MemberId(5)),
            "Joining is never rank 1"
        );
        let got = now
            .owners_assuming(MemberId(5), MemberState::Active, d)
            .expect("member 5 exists");
        assert_eq!(got, promoted.write_owners(d));
        assert_eq!(got.len(), 3);
        if got.contains(&MemberId(5)) {
            owned += 1;
        }
        if got.first() == Some(&MemberId(5)) {
            first += 1;
        }
    }
    // 3/5 of 4000 = 2400 own it; 1/5 = 800 have it first.
    assert!((2100..=2700).contains(&owned), "owned {owned}");
    assert!((600..=1000).contains(&first), "first {first}");
}

// Catches: a Draining assumption that still lists the member as a write owner.
#[test]
fn a_member_assumed_draining_owns_nothing() {
    let now = current(3, &R3_FARM);
    for d in &digests(500) {
        let got = now
            .owners_assuming(MemberId(1), MemberState::Draining, d)
            .expect("member 1 exists");
        assert!(!got.contains(&MemberId(1)));
    }
}

// Catches: the read side of the assumption disagreeing with the hand-built map (the
// promoted joiner must be read in rank order among the settled owners, not last).
#[test]
fn the_read_order_assuming_active_is_the_promoted_maps_read_order() {
    let now = current(3, &R3_FARM);
    let promoted = with_state(3, &R3_FARM, 5, MemberState::Active);
    for d in &digests(1000) {
        let got = now
            .read_order_assuming(MemberId(5), MemberState::Active, d)
            .expect("member 5 exists");
        assert_eq!(got, promoted.read_order(d));
    }
}

// Catches: an unknown id answered with an owner list (it would claim a member that the map
// does not have), and an `assuming` that alters the original or the generation.
#[test]
fn an_unknown_member_is_none_and_assuming_changes_only_the_state() {
    let now = current(3, &R3_FARM);
    let d = &digests(1)[0];
    assert!(
        now.owners_assuming(MemberId(99), MemberState::Active, d)
            .is_none()
    );
    assert!(
        now.read_order_assuming(MemberId(99), MemberState::Active, d)
            .is_none()
    );
    assert!(now.assuming(MemberId(99), MemberState::Active).is_none());
    let assumed = now
        .assuming(MemberId(5), MemberState::Active)
        .expect("member 5");
    assert_eq!(assumed.generation(), now.generation());
    assert_eq!(
        assumed.member(MemberId(5)).map(|m| m.state),
        Some(MemberState::Active)
    );
    assert_eq!(
        now.member(MemberId(5)).map(|m| m.state),
        Some(MemberState::Joining)
    );
    assert_eq!(assumed.members().len(), now.members().len());
}
