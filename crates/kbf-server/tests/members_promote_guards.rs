//! The guards of the joiner's promotion (`kbf_server::members`, `Members::with_handoff`;
//! `docs/design/rebalance-handoff.md`, sections 4 and 7): each condition and each
//! compare-and-set of the promoting beat has a test in which it is the only thing standing
//! between the joiner and a promotion it must not get. The story is the one of
//! `members_auto_promote.rs` (its scene is shared): two members that formed a farm, a
//! third that joined it, the nodes' reports made by the tests.
//!
//! The tests that interleave another writer use [`hooked::Hooked::after_get`], which runs
//! the other writer right after the beat's read of a key: the answer the beat keeps is
//! stale when it commits.

#[path = "support/hooked.rs"]
mod hooked;
#[path = "support/promote_scene.rs"]
mod promote_scene;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kbf_map::MemberState;
use kbf_proto::kbf::map::v1 as pb;
use kbf_server::members::{Block, Progress};
use kbf_server::storage_health::Reported;
use kbf_state::{Cond, Store, Txn};
use promote_scene::{SECOND, SETTLE, flushed_to, handoff, members, node, scene, state_of};

/// A row's member and nothing else: appended to a stored row, it overrides the member's
/// fields that it sets (a protobuf message that is decoded twice is merged), which is how
/// a test writes "the state of this row" without knowing the rest of its encoding.
#[derive(Clone, PartialEq, prost::Message)]
struct OnlyMember {
    #[prost(message, optional, tag = "1")]
    member: Option<pb::Member>,
}

/// Catches: a joiner promoted on an acknowledgement of the old epoch because a write
/// owner other than the joiner restarted. The restart is in the map (the registering
/// frontend published it) but no fingerprint shows it (a boot is not part of the placement),
/// and the one that reopens the epoch is the sweeper, which has not run: the promoting beat
/// must refuse on the boots alone. It is the only thing wrong: the joiner is one second
/// short of promoted, the owner restarts, the next beat would promote. Then the sweeper runs
/// and the join completes under the newer epoch. Mutant seen red: the comparison of the
/// fence's boots with the map's removed from the beat (promoted at that beat).
#[test]
fn a_restarted_owner_that_is_not_the_joiner_blocks_the_beat_until_the_epoch_is_reopened() {
    let s = scene();
    let epoch = s.nearly_promoted();
    let plain = members(&s.store, 2, None);
    let again = plain.register(&node(1)).expect("an owner restarts");
    assert_eq!(again.id, s.a.id);
    assert_eq!(again.boot, s.a.boot + 1);
    assert_eq!(
        s.map().member(s.a.id).map(|m| m.boot),
        Some(again.boot),
        "the restart is in the map"
    );
    assert_eq!(
        s.m.tail_epoch().expect("epoch"),
        epoch,
        "nothing reopened it"
    );

    s.joiner_after(SECOND, s.done(epoch));
    assert_eq!(s.state(), Some(MemberState::Joining));
    assert!(matches!(
        s.blocked().as_slice(),
        [Block::OwnerBoot { member, have: Some(have), fenced: Some(fenced) }]
            if *member == s.a.id && *have == again.boot && *fenced == s.a.boot
    ));
    // However long it waits, until the epoch is reopened.
    s.joiner_after(SETTLE * 2, s.done(epoch));
    assert_eq!(s.state(), Some(MemberState::Joining));

    // The sweeper reopens it; the joiner acknowledges the newer epoch; the join completes.
    s.m.refresh().expect("refresh");
    let reopened = s.m.tail_epoch().expect("epoch");
    assert_eq!(reopened, epoch + 1);
    s.beat_joiner(Progress::CaughtUp, s.done(reopened));
    s.joiner_after(SETTLE, s.done(reopened));
    assert_eq!(s.state(), Some(MemberState::Active));
}

/// Catches: the flush barrier opening the fence while a member is `Down` on a live row, so
/// that only its (fresh) report speaks for it: a planned restart began after the member
/// reported it had flushed, and what it stages while it is down is flushed by nobody. The
/// `Down` check is the only blocker: every member's report is fresh and at the generation
/// the join needs. Then the plan is undone and the fence opens. Mutant seen red: the `Down`
/// branch of the barrier removed (the epoch opens at the third report).
#[test]
fn a_member_planned_down_after_its_fresh_flush_report_still_holds_the_fence_shut() {
    let s = scene();
    let g = s.map().generation();
    s.beat(&s.m, &s.a, Progress::CaughtUp, flushed_to(g));
    s.beat(&s.m, &s.b, Progress::CaughtUp, flushed_to(g));
    s.m.plan_down(s.a.id, Duration::from_secs(600))
        .expect("plan down");
    assert_eq!(state_of(&s.map(), s.a.id), Some(MemberState::Down));
    s.beat(&s.m, &s.c, Progress::CaughtUp, flushed_to(g));
    s.m.refresh().expect("refresh");
    assert_eq!(s.m.tail_epoch().expect("epoch"), 0, "Down holds the fence");
    assert_eq!(s.blocked(), [Block::Down(s.a.id)]);

    assert!(s.m.undo_plan(s.a.id).expect("undo"));
    assert_eq!(s.m.tail_epoch().expect("epoch"), 1, "its report was fresh");
}

/// Catches: a member that left the map after the fence opened (its dwell ran out) going
/// unrecorded when the frontend that advances marks was not the one that watched it go
/// `Down`: a frontend without this feature refreshed the map through the whole dwell, so
/// when the handoff frontend first looks the member is no longer `Down` anywhere, and only
/// the fence's recorded boots remember it. The join must be degraded then, and withheld for
/// as long as the joiner keeps reporting complete. Mutant seen red: the clause of the
/// fence step that marks the join degraded when a recorded owner left the map removed
/// (promoted after the settle time).
#[test]
fn an_owner_that_left_the_map_unseen_by_the_sweeper_degrades_the_join() {
    let s = scene();
    let epoch = s.open_fence();
    let plain = members(&s.store, 2, None);
    // Member 1 stops beating; only the plain frontend runs, through the whole dwell.
    for _ in 0..8 {
        s.store.clock.advance(Duration::from_secs(1800));
        for who in [&s.b, &s.c] {
            s.beat(&plain, who, Progress::CaughtUp, s.done(epoch));
        }
        plain.refresh().expect("refresh");
    }
    assert!(s.map().member(s.a.id).is_none(), "member 1 left");
    assert_eq!(s.m.tail_epoch().expect("epoch"), epoch, "no one advanced");

    s.m.refresh().expect("refresh");
    let newer = s.m.tail_epoch().expect("epoch");
    assert_eq!(newer, epoch + 1, "the placement changed: a newer epoch");
    assert!(s.blocked().contains(&Block::Degraded));
    s.beat_joiner(Progress::CaughtUp, s.done(newer));
    s.joiner_after(SETTLE * 2, s.done(newer));
    s.joiner_after(SETTLE * 2, s.done(newer));
    assert_eq!(s.state(), Some(MemberState::Joining));
    assert_eq!(s.blocked(), [Block::Degraded]);
}

/// Catches: a promotion committed over a row that changed after the beat read it. An
/// operator's drain is half done (its row is written, its map not yet published: the two
/// are separate commits): the map and the mark are as the beat read them, so only the
/// version of the row stands in the way. The beat reads `Draining` at its next attempt and
/// promotes nothing. Mutant seen red: the promoting row written without the version it
/// was read at (the joiner is `Active` after the drain was published).
#[test]
fn a_row_that_changed_under_the_promoting_beat_is_not_overwritten() {
    let s = scene();
    let epoch = s.nearly_promoted();
    let fake = Arc::clone(&s.store.fake);
    s.store.after_get("member/3", move || {
        let row = fake.get("member/3").expect("read").expect("a row");
        let mut value = row.value;
        value.extend(prost::Message::encode_to_vec(&OnlyMember {
            member: Some(pb::Member {
                state: pb::MemberState::Draining as i32,
                ..pb::Member::default()
            }),
        }));
        fake.txn(Txn::new().put("member/3", value, Cond::Any))
            .expect("store")
            .expect("commit");
    });
    s.joiner_after(SECOND, s.done(epoch));
    s.m.refresh().expect("refresh");
    assert_eq!(s.state(), Some(MemberState::Draining));
    assert!(s.store.fake.get("unfilled/3").expect("read").is_some());
}

/// Catches: a promotion committed on a map that moved after the beat judged it. A member
/// is planned down by an operator between the beat's read of the map and its commit: the
/// row of the joiner and its mark are as the beat read them, so only the check of the
/// map's version stands in the way. The beat judges again and finds a member `Down`.
/// Mutant seen red: the check of the map's version removed from the promoting write (the
/// joiner is `Active` with a member `Down`).
#[test]
fn a_map_that_moved_under_the_promoting_beat_is_judged_again() {
    let s = scene();
    let epoch = s.nearly_promoted();
    let operator = members(&s.store, 2, None);
    let a = s.a.id;
    // What the joiner reports is read from the map: before the hook is armed, which the
    // first read of the map would take.
    let fill = s.done(epoch);
    s.store.after_get("map", move || {
        operator
            .plan_down(a, Duration::from_secs(600))
            .expect("plan down");
    });
    s.joiner_after(SECOND, fill);
    assert_eq!(s.state(), Some(MemberState::Joining));
    assert!(s.blocked().contains(&Block::Down(s.a.id)));
    assert!(s.store.fake.get("unfilled/3").expect("read").is_some());
}

/// Catches: a promotion committed on a mark the sweeper rewrote after the beat read it. A
/// planned restart of member 1 begins and ends between the beat's read of the mark and its
/// read of the map: the sweeper saw it `Down` (a newer epoch, the join degraded) but the
/// map the beat then reads is back to the placement of the mark it holds, and the joiner's
/// row and the map's version are as read. Only the mark's version stands in the way. The
/// beat judges again by the new mark. Mutant seen red: the mark deleted without the
/// version it was read at (the joiner is `Active` on the old epoch, with the join
/// degraded).
#[test]
fn a_mark_the_sweeper_rewrote_under_the_promoting_beat_stops_the_promotion() {
    let s = scene();
    let epoch = s.nearly_promoted();
    let operator = members(&s.store, 2, None);
    let sweeper = Arc::clone(&s.m);
    let a = s.a.id;
    s.store.after_get("unfilled/3", move || {
        operator
            .plan_down(a, Duration::from_secs(600))
            .expect("plan down");
        sweeper.refresh().expect("the fence reopens");
        assert!(operator.undo_plan(a).expect("undo"));
    });
    s.joiner_after(SECOND, s.done(epoch));
    assert_eq!(s.m.tail_epoch().expect("epoch"), epoch + 1, "it reopened");
    assert_eq!(s.state(), Some(MemberState::Joining));
    let blocked = s.blocked();
    assert!(blocked.contains(&Block::Degraded), "{blocked:?}");
    assert!(
        blocked
            .iter()
            .any(|b| matches!(b, Block::TailPending { .. }))
    );
    assert!(s.store.fake.get("unfilled/3").expect("read").is_some());
}

/// Catches: two frontends that get the same beat both promoting the joiner. The second
/// one's beat runs between the first one's read and its commit and promotes; the first
/// one's write, made on what it read, must not commit: the farm's generation moves once
/// and `meta/members_formed` is written once. (The row's version, the mark's and the
/// map's each hold the second write back: one removed leaves the test green, all three
/// removed turn it red; the tests above take them one at a time.) Mutant seen red: the
/// promoting write without any of the three guards (written twice).
#[test]
fn two_frontends_that_get_the_same_beat_promote_the_joiner_once() {
    let s = scene();
    let epoch = s.nearly_promoted();
    let before = s.map().generation();
    let second = members(&s.store, 2, Some(handoff()));
    let formed = Arc::new(AtomicU64::new(0));
    let (who, boot, fill) = (s.c.id, s.c.boot, s.done(epoch));
    let (fake, seen) = (Arc::clone(&s.store.fake), Arc::clone(&formed));
    s.store.after_get("unfilled/3", move || {
        second
            .heartbeat_with(
                who,
                boot,
                Progress::CaughtUp,
                Reported::default(),
                fill,
                None,
            )
            .expect("the other frontend's beat");
        let version = fake.get("meta/members_formed").expect("read");
        seen.store(version.expect("formed").version.0, Ordering::SeqCst);
    });
    s.joiner_after(SECOND, fill);
    assert_eq!(s.state(), Some(MemberState::Active));
    assert_eq!(s.map().generation(), before + 1, "one promotion");
    let version = s.store.fake.get("meta/members_formed").expect("read");
    assert_eq!(
        version.expect("formed").version.0,
        formed.load(Ordering::SeqCst),
        "written once"
    );
    assert!(s.store.fake.get("unfilled/3").expect("read").is_none());
}
