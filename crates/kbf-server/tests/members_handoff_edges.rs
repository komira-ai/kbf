//! Edges of a joiner's handoff (`kbf_server::members`, `docs/design/rebalance-handoff.md`,
//! sections 4 and 6) that the story tests of `members_auto_promote.rs` do not reach: a
//! farm that has no map yet, a mark nobody dated, a mark whose settle time was never
//! started, a mark whose map is gone, a store that conflicts or holds a bad epoch. Every
//! test runs on the fake store with its injectable clock.

#[path = "support/hooked.rs"]
mod hooked;
#[path = "support/promote_scene.rs"]
mod promote_scene;

use std::sync::atomic::Ordering;

use kbf_map::{MemberId, MemberState};
use kbf_server::members::{Block, MembershipError, Progress};
use kbf_state::{Cond, Store, Txn};
use promote_scene::{SETTLE, handoff, members, scene};

use hooked::Hooked;

fn put(s: &promote_scene::Scene, key: &str, value: &[u8]) {
    let txn = Txn::new().put(key, value.to_vec(), Cond::Any);
    assert!(s.store.fake.txn(txn).expect("write").is_ok());
}

/// Catches: a farm with no map failing the operator's question or inventing a reason: with
/// the setting on and nothing stored, no joiner is blocked; and a store that fails the read
/// is an error, not an empty answer.
#[test]
fn a_farm_with_no_map_has_no_blocked_joiner_and_a_failing_store_is_an_error() {
    let store = Hooked::new();
    let m = members(&store, 2, Some(handoff()));
    assert!(m.blocked_joiners().expect("read").is_empty());
    assert!(m.blocked_on(MemberId(1)).expect("read").is_empty());
    store.fail_gets.store(1, Ordering::SeqCst);
    assert!(m.blocked_joiners().is_err());
}

/// Catches: a mark nobody dated yet (written empty at the join) judged against generation 0,
/// which every report passes: until the sweeper dates it, the barrier is the generation of
/// the map now, so every member that has not flushed it is named with that generation.
#[test]
fn an_undated_mark_is_judged_against_the_generation_of_the_map_now() {
    let s = scene();
    let generation = s.map().generation();
    let blocks = s.blocked();
    assert!(!blocks.is_empty());
    for block in &blocks {
        assert!(
            matches!(block, Block::Flushing { need, .. } if *need == generation),
            "{block:?} is not judged against generation {generation}"
        );
    }
}

/// Catches: a joiner whose own conditions all hold but whose settle time was never started
/// (its beat said it was still rebuilding, so nothing was stamped) reported as free to
/// promote, and one whose settle time has run out reported as still settling. The first
/// owes the whole settle time; the second owes nothing.
#[test]
fn a_settle_time_not_started_is_owed_whole_and_one_run_out_is_not_owed() {
    let s = scene();
    let epoch = s.open_fence();
    s.beat_joiner(Progress::CatchingUp, s.done(epoch));
    assert_eq!(s.state(), Some(MemberState::Joining));
    assert_eq!(s.blocked(), vec![Block::Settling(SETTLE)]);

    // Now the settle time starts, and runs out with no beat to promote the joiner.
    s.beat_joiner(Progress::CaughtUp, s.done(epoch));
    s.store.clock.advance(SETTLE * 2);
    assert_eq!(s.state(), Some(MemberState::Joining));
    assert!(s.blocked().is_empty());
}

/// Catches: a joiner's beat that promotes, or fails, when its mark is gone: it changes
/// nothing, and the joiner stays `Joining` for an operator to settle.
#[test]
fn a_joiner_whose_mark_is_gone_is_not_promoted_by_its_beat() {
    let s = scene();
    let epoch = s.open_fence();
    let txn = Txn::new().delete("unfilled/3", Cond::Any);
    assert!(s.store.fake.txn(txn).expect("delete").is_ok());
    s.beat_joiner(Progress::CaughtUp, s.done(epoch));
    s.joiner_after(SETTLE * 2, s.done(epoch));
    assert_eq!(s.state(), Some(MemberState::Joining));
}

/// Catches: the sweeper acting on a mark whose map is not stored (it would invent a fence
/// or fail the refresh): the mark is left as written and nothing is opened.
#[test]
fn the_sweeper_leaves_a_mark_when_no_map_is_stored() {
    let store = Hooked::new();
    let m = members(&store, 2, Some(handoff()));
    let txn = Txn::new().put("unfilled/5", Vec::new(), Cond::Any);
    assert!(store.fake.txn(txn).expect("write").is_ok());
    m.refresh().expect("refresh");
    assert_eq!(m.tail_epoch().expect("epoch"), 0);
    assert!(store.fake.get("unfilled/5").expect("read").is_some());
}

/// Catches: a refused write of the fence (another frontend won the epoch) counted as an
/// opened fence or retried for ever: the epoch stays where it was, the refresh still
/// succeeds, and the next refresh once the store accepts opens it.
#[test]
fn a_fence_the_store_refuses_is_not_opened_and_is_opened_by_the_next_sweep() {
    let s = scene();
    s.flush_all(s.map().generation());
    s.store.conflict_on(Some("meta/tail_epoch"));
    s.m.refresh().expect("refresh");
    assert_eq!(s.m.tail_epoch().expect("epoch"), 0);
    s.store.conflict_on(None);
    s.m.refresh().expect("refresh");
    assert!(s.m.tail_epoch().expect("epoch") > 0);
}

/// Catches: a stored tail epoch that is not eight bytes failing the whole refresh (and with
/// it the map every node reads): the refresh still answers with the map, the fault is said
/// by `tail_epoch`, and no fence opens.
#[test]
fn a_corrupt_tail_epoch_does_not_fail_the_refresh() {
    let s = scene();
    s.flush_all(s.map().generation());
    put(&s, "meta/tail_epoch", &[1, 2, 3]);
    let map = s.m.refresh().expect("refresh").expect("a map");
    assert_eq!(map.generation(), s.map().generation());
    assert!(matches!(s.m.tail_epoch(), Err(MembershipError::Corrupt(_))));
    assert_eq!(s.state(), Some(MemberState::Joining));
}
