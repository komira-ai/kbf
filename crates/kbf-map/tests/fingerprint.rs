//! `Map::placement_fingerprint`: changes exactly when placement changes.

mod common;

use common::{map, member};
use kbf_map::{DenyEntry, Map, Member, MemberId, MemberState, Rule};

fn base_members() -> Vec<Member> {
    vec![
        member(1, 1, MemberState::Active),
        member(2, 2, MemberState::Active),
        member(3, 1, MemberState::Joining),
    ]
}

fn fp(members: Vec<Member>) -> u64 {
    map(5, 2, members).placement_fingerprint()
}

fn edited(f: impl FnOnce(&mut Vec<Member>)) -> u64 {
    let mut m = base_members();
    f(&mut m);
    fp(m)
}

// Catches: a fingerprint that depends on the generation, the address, the boot counter, the
// protocol floor, the deny list, the order members were listed in, or on Joining versus
// CatchingUp (placement treats both alike): a fill report would then never match.
#[test]
fn irrelevant_fields_do_not_change_the_fingerprint() {
    let want = fp(base_members());
    assert_eq!(want, fp(base_members()), "deterministic");

    let other_gen = map(99, 2, base_members()).placement_fingerprint();
    assert_eq!(want, other_gen, "generation");

    assert_eq!(
        want,
        edited(|m| m[0].addr = "elsewhere:1".into()),
        "address"
    );
    assert_eq!(want, edited(|m| m[1].boot = 77), "boot");
    assert_eq!(want, edited(|m| m.reverse()), "member order");
    assert_eq!(
        want,
        edited(|m| m[2].state = MemberState::CatchingUp),
        "joining vs catching up"
    );

    let denied = vec![DenyEntry::Node("node-x".into())];
    let m = Map::new(5, base_members(), Rule::Rendezvous, 2, 9, denied).expect("a map");
    assert_eq!(
        want,
        m.placement_fingerprint(),
        "min protocol and deny list"
    );
}

// Catches: a fingerprint blind to any one of the placement inputs. Each edit moves digests
// between members, so each must change the value, and no two of them may agree.
#[test]
fn every_placement_change_changes_the_fingerprint() {
    let base = fp(base_members());
    let changes = [
        (
            "add a member",
            edited(|m| m.push(member(4, 1, MemberState::Active))),
        ),
        ("remove a member", edited(|m| drop(m.pop()))),
        (
            "joining to active",
            edited(|m| m[2].state = MemberState::Active),
        ),
        (
            "active to draining",
            edited(|m| m[0].state = MemberState::Draining),
        ),
        ("active to down", edited(|m| m[0].state = MemberState::Down)),
        ("weight", edited(|m| m[0].weight = 5)),
        ("zone", edited(|m| m[0].zone = "other-zone".into())),
        ("id", edited(|m| m[2].id = MemberId(30))),
        (
            "replication",
            map(5, 3, base_members()).placement_fingerprint(),
        ),
    ];
    let mut seen = vec![base];
    for (what, value) in changes {
        assert!(
            !seen.contains(&value),
            "{what} did not change the fingerprint"
        );
        seen.push(value);
    }
}

// Catches: ambiguity in the byte layout: two zones that concatenate alike must not collide.
#[test]
fn zone_boundaries_are_part_of_the_fingerprint() {
    let a = fp(vec![
        {
            let mut m = member(1, 1, MemberState::Active);
            m.zone = "ab".into();
            m
        },
        {
            let mut m = member(2, 1, MemberState::Active);
            m.zone = "c".into();
            m
        },
    ]);
    let b = fp(vec![
        {
            let mut m = member(1, 1, MemberState::Active);
            m.zone = "a".into();
            m
        },
        {
            let mut m = member(2, 1, MemberState::Active);
            m.zone = "bc".into();
            m
        },
    ]);
    assert_ne!(a, b);
}

// Catches: the check the frontend makes: the joiner's report is for the map where it is
// Active; `assuming` must yield exactly that fingerprint.
#[test]
fn the_assumed_fingerprint_is_the_promoted_maps() {
    let now = map(5, 2, base_members());
    let promoted = edited(|m| m[2].state = MemberState::Active);
    let assumed = now
        .assuming(MemberId(3), MemberState::Active)
        .expect("member 3");
    assert_eq!(assumed.placement_fingerprint(), promoted);
    assert_ne!(assumed.placement_fingerprint(), now.placement_fingerprint());
}

// Catches: a change of the computation across builds (it is compared between processes of
// different versions). A new layout is a new function, not an edit of this one.
#[test]
fn the_fingerprint_is_pinned() {
    assert_eq!(fp(base_members()), 0xFF58_1B77_EFF0_5837);
}
