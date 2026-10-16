//! Placement: who owns a digest, and how that moves with the map.
//!
//! Each test names the defect it catches; the mutant that was planted to watch it fail
//! is in the pull request.

mod common;

use common::{digests, ids, map, member};
use kbf_map::{Map, MemberId, MemberState};
use kbf_types::{Digest, DigestFunction};

use MemberState::{Active, CatchingUp, Down, Draining, Joining};

/// Six members of unequal weight, all active.
const SIX: [(u64, u32); 6] = [(11, 1), (12, 1), (13, 2), (14, 3), (15, 1), (16, 2)];

fn six(r: u8, skip: Option<u64>) -> Map {
    let members = SIX
        .iter()
        .filter(|(id, _)| Some(*id) != skip)
        .map(|&(id, w)| member(id, w, Active))
        .collect();
    map(1, r, members)
}

fn without(list: &[MemberId], id: MemberId) -> Vec<MemberId> {
    list.iter().copied().filter(|m| *m != id).collect()
}

/// `after` is `before` with `gone` taken out and the next member appended, or `before`
/// itself when `gone` was not an owner.
fn assert_only_gone_blobs_move(before: &[MemberId], after: &[MemberId], gone: MemberId) {
    if before.contains(&gone) {
        let kept = without(before, gone);
        assert_eq!(
            &after[..kept.len()],
            &kept[..],
            "owners that stay keep their rank"
        );
        assert_eq!(after.len(), before.len());
        assert!(
            !before.contains(after.last().unwrap()),
            "a new owner fills the slot"
        );
    } else {
        assert_eq!(before, after, "a digest {gone} did not own must not move");
    }
}

// Catches: a placement that reshuffles digests when membership changes (a modulo or a
// ring with shared state), and a score that depends on the other members. Exact per
// digest, for removal and for the reverse (adding), at R=1 and R=2.
#[test]
fn removing_or_adding_a_member_moves_only_its_blobs() {
    for r in [1, 2] {
        for &(gone, _) in &SIX {
            let (full, short) = (six(r, None), six(r, Some(gone)));
            let mut owned = 0;
            for d in digests(20_000) {
                let (before, after) = (full.write_owners(&d), short.write_owners(&d));
                owned += usize::from(before.contains(&MemberId(gone)));
                assert_only_gone_blobs_move(&before, &after, MemberId(gone));
            }
            assert!(
                owned > 0,
                "member {gone} owned nothing: the test would be vacuous"
            );
        }
    }
}

// Catches: the same property over the 100,000 digests of the plan, once, for one member.
#[test]
fn removal_at_one_hundred_thousand_digests() {
    let (full, short) = (six(2, None), six(2, Some(14)));
    for d in digests(100_000) {
        let before = full.write_owners(&d);
        assert_only_gone_blobs_move(&before, &short.write_owners(&d), MemberId(14));
    }
}

// Catches: a score that ignores the weight (every member then gets an equal share).
#[test]
fn weights_set_the_share_of_primaries() {
    let m = map(1, 1, vec![member(1, 1, Active), member(2, 2, Active)]);
    let n = 90_000;
    let heavy = digests(n)
        .iter()
        .filter(|d| m.write_owners(d) == ids(&[2]))
        .count();
    let share = heavy as f64 / n as f64;
    assert!(
        (share - 2.0 / 3.0).abs() < 0.01,
        "weight-2 share {share}, want 2/3 +- 0.01"
    );
}

// Catches: a score that does not mix in the member id (all members then score alike and
// the lowest id wins everything).
#[test]
fn equal_members_share_evenly() {
    let m = map(1, 1, (1..=6).map(|i| member(i, 1, Active)).collect());
    let n = 60_000;
    let mut wins = [0usize; 6];
    for d in digests(n) {
        wins[(m.write_owners(&d)[0].0 - 1) as usize] += 1;
    }
    for (i, w) in wins.iter().enumerate() {
        let share = *w as f64 / n as f64;
        assert!(
            (share - 1.0 / 6.0).abs() < 0.01,
            "member {} has share {share}",
            i + 1
        );
    }
}

fn digest(hash: [u8; 32], size: u64) -> Digest {
    Digest::new(DigestFunction::Sha256, hash, size)
}

// Catches: any change to the hash, its seed, the byte layout or the score formula. The
// lists were produced by an independent implementation (Python, `math.log`), not by this
// crate.
#[test]
fn rankings_are_pinned() {
    let m = six(3, None);
    let cases = [
        (digest([0; 32], 0), [11, 16, 13]),
        (digest([0xff; 32], 1), [13, 16, 14]),
        (digest(std::array::from_fn(|i| i as u8), 4096), [13, 16, 11]),
        (
            digest(std::array::from_fn(|i| 32 + i as u8), 1 << 40),
            [16, 13, 12],
        ),
        (digest([7; 32], 123_456), [16, 14, 13]),
    ];
    for (d, want) in cases {
        assert_eq!(m.write_owners(&d), ids(&want), "digest {d}");
    }
}

// Catches: a size that is ignored (two digests with one hash and two sizes share an
// owner list for every hash) or a platform-dependent order of equal scores.
#[test]
fn the_size_is_part_of_the_key() {
    let m = six(3, None);
    let moved = (0..200u8)
        .filter(|i| m.write_owners(&digest([*i; 32], 1)) != m.write_owners(&digest([*i; 32], 2)))
        .count();
    assert!(
        moved > 50,
        "only {moved} of 200 owner lists changed with the size"
    );
}

/// Five members, the sixth (`X`) in `state`; compare against the map without it and the
/// map with it active.
const X: u64 = 13;

fn six_with(state: MemberState) -> Map {
    let members = SIX
        .iter()
        .map(|&(id, w)| member(id, w, if id == X { state } else { Active }))
        .collect();
    map(2, 2, members)
}

/// `base` then the members of `extra` it lacks.
fn then(base: &[MemberId], extra: &[MemberId]) -> Vec<MemberId> {
    let mut out = base.to_vec();
    out.extend(extra.iter().copied().filter(|m| !base.contains(m)));
    out
}

// Catches (rows Joining and CatchingUp of the table): a joiner that is not a write owner
// at once, or one read before the members that hold the data. Read order is the owners
// without the joiner, then the joiner (and whoever else it displaced from the write set).
#[test]
fn a_joining_node_takes_writes_at_once_and_is_read_last() {
    let x = MemberId(X);
    for state in [Joining, CatchingUp] {
        let (m, active) = (six_with(state), six_with(Active));
        let without_x = six(2, Some(X));
        let mut joined = 0;
        for d in digests(5_000) {
            assert_eq!(m.write_owners(&d), active.write_owners(&d));
            let base = without_x.write_owners(&d);
            let order = m.read_order(&d);
            assert_eq!(order, then(&base, &m.write_owners(&d)), "{state:?}");
            if m.write_owners(&d).contains(&x) {
                joined += 1;
                assert_eq!(order.last(), Some(&x), "{state:?} is read last");
            } else {
                assert_eq!(order, base);
            }
        }
        assert_ne!(joined, 0, "{state:?} owned nothing");
    }
}

// Catches (row Active): an active member that is not first-class for both.
#[test]
fn an_active_node_reads_and_writes_in_rank_order() {
    let m = six_with(Active);
    for d in digests(2_000) {
        assert_eq!(m.write_owners(&d), m.read_order(&d));
        assert_eq!(m.write_owners(&d).len(), 2);
    }
}

// Catches (row Draining): a draining member that still takes writes, or one read after
// its successor (which has no data yet).
#[test]
fn a_draining_node_takes_no_writes_and_is_read_before_its_successor() {
    let x = MemberId(X);
    let (m, active, without_x) = (six_with(Draining), six_with(Active), six(2, Some(X)));
    let mut held = 0;
    for d in digests(5_000) {
        let write = m.write_owners(&d);
        assert_eq!(
            write,
            without_x.write_owners(&d),
            "its successor takes the slot"
        );
        assert!(!write.contains(&x));
        let order = m.read_order(&d);
        assert_eq!(order, then(&active.write_owners(&d), &write));
        held += usize::from(order.contains(&x));
    }
    assert_ne!(held, 0, "the draining member was never read");
}

// Catches (row Down): a down member whose slot is given away at once (flapping would
// remap its blobs twice), or one still listed for reading.
#[test]
fn a_down_node_keeps_its_write_slot_and_is_not_read() {
    let x = MemberId(X);
    let (m, active) = (six_with(Down), six_with(Active));
    let mut kept = 0;
    for d in digests(5_000) {
        assert_eq!(m.write_owners(&d), active.write_owners(&d));
        assert_eq!(m.read_order(&d), without(&active.write_owners(&d), x));
        kept += usize::from(m.write_owners(&d).contains(&x));
    }
    assert_ne!(kept, 0, "the down member owned nothing");
}

// Catches: a write set padded with a member that cannot take writes.
#[test]
fn too_few_writers_give_fewer_owners() {
    let m = map(1, 2, vec![member(1, 1, Active), member(2, 1, Draining)]);
    let d = digest([3; 32], 3);
    assert_eq!(m.write_owners(&d), ids(&[1]));
    let mut order = m.read_order(&d);
    order.sort();
    assert_eq!(
        order,
        ids(&[1, 2]),
        "both hold data; neither is listed twice"
    );
}

// Catches: a lookup across generations that forgets the previous owners. A member that
// left between the generations is still tried, after the current owners.
#[test]
fn the_previous_generation_is_read_after_the_current_one() {
    let (prev, cur) = (six(2, None), six(2, Some(14)));
    let mut appended = 0;
    for d in digests(5_000) {
        let now = cur.read_order(&d);
        assert_eq!(cur.read_order_with_previous(None, &d), now);
        let across = cur.read_order_with_previous(Some(&prev), &d);
        assert_eq!(across, then(&now, &prev.read_order(&d)));
        assert_eq!(&across[..now.len()], &now[..]);
        appended += across.len() - now.len();
    }
    assert_ne!(appended, 0, "no digest needed the previous generation");
}
