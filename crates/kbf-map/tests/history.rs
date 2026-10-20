//! `Map::read_order_with_history`: the read order across retained generations.

mod common;

use common::{digests, ids, map, member};
use kbf_map::{Map, MemberId, MemberState};

fn farm(generation: u64, of: &[u64]) -> Map {
    let members = of
        .iter()
        .map(|&i| member(i, 1, MemberState::Active))
        .collect();
    map(generation, 2, members)
}

fn order_ids(order: &[&kbf_map::Member]) -> Vec<MemberId> {
    order.iter().map(|m| m.id).collect()
}

// Catches: history that changes the answer when there is none.
#[test]
fn no_history_is_the_current_read_order() {
    let now = farm(3, &[1, 2, 3]);
    for d in &digests(200) {
        assert_eq!(
            order_ids(&now.read_order_with_history(&[], d)),
            now.read_order(d)
        );
    }
}

// Catches: a different order from the one-deep function it generalises.
#[test]
fn one_previous_map_is_read_order_with_previous() {
    let now = farm(3, &[1, 2, 3, 4]);
    let prev = farm(2, &[1, 2, 3]);
    for d in &digests(300) {
        let want = order_ids(&now.read_order_with_previous(Some(&prev), d));
        assert_eq!(order_ids(&now.read_order_with_history(&[&prev], d)), want);
    }
}

// Catches: a member listed once per map that holds it (a duplicate probe), current-last
// ordering, and history listed oldest first.
#[test]
fn the_current_map_comes_first_and_nobody_is_listed_twice() {
    let now = farm(4, &[1, 2, 3, 4]);
    let newer = farm(3, &[1, 2, 3, 5]);
    let older = farm(2, &[1, 2, 3, 6]);
    for d in &digests(500) {
        let got = order_ids(&now.read_order_with_history(&[&newer, &older], d));
        let mut unique = got.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), got.len(), "duplicate in {got:?}");

        let cur = now.read_order(d);
        assert_eq!(got[..cur.len()], cur[..], "current first");

        // After the current members: the newer map's, then the older map's.
        let rest: Vec<MemberId> = got[cur.len()..].to_vec();
        let mut expect = Vec::new();
        for m in newer.read_order(d).into_iter().chain(older.read_order(d)) {
            if !cur.contains(&m) && !expect.contains(&m) {
                expect.push(m);
            }
        }
        assert_eq!(rest, expect);
    }
}

// Catches: a member that left two generations ago being forgotten: a blob written then is
// found only through it, and each member is the entry of the map that holds it (its address
// there).
#[test]
fn a_member_that_left_two_generations_ago_is_still_listed_with_its_old_entry() {
    let now = farm(4, &[1, 2, 3]);
    let newer = farm(3, &[1, 2, 3]);
    let mut older_members: Vec<_> = [1u64, 2, 3]
        .iter()
        .map(|&i| member(i, 1, MemberState::Active))
        .collect();
    older_members.push({
        let mut m = member(9, 1000, MemberState::Active);
        m.addr = "old-host:9".into();
        m
    });
    let older = map(2, 2, older_members);
    let mut found = 0;
    for d in &digests(300) {
        let got = now.read_order_with_history(&[&newer, &older], d);
        if let Some(m) = got.iter().find(|m| m.id == MemberId(9)) {
            assert_eq!(m.addr, "old-host:9");
            found += 1;
        }
        assert!(
            !now.read_order_with_previous(Some(&newer), d)
                .iter()
                .any(|m| m.id == MemberId(9))
        );
    }
    assert!(
        found > 100,
        "the heavy old member owns most digests; found {found}"
    );
    let _ = ids(&[]);
}
