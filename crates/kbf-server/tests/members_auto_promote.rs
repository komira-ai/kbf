//! Automatic promotion of a joiner (`kbf_server::members`, `Members::with_handoff`;
//! `docs/design/rebalance-handoff.md`, sections 3, 4 and 7): a node that joined a farm
//! holding data is promoted by its own heartbeat once it is filled, and by nothing else.
//! Every test runs on the fake store with its injectable clock. The farm is two members
//! that formed it and a third that joined it; the nodes' reports are made by the tests,
//! each saying what its node would: [`Fill`] (flush generation, tail epoch acknowledged,
//! fingerprint) and whether the index rebuild is done.
//!
//! Each condition has a test in which it is the only one that fails and promotion is
//! withheld, then given and promoted: a test that only withholds could pass for any reason.
//! The fence tests run the same story with the condition's check removed, so
//! each fails when its check is missing (the test comments name the defects).

#[path = "support/hooked.rs"]
mod hooked;
#[path = "support/promote_scene.rs"]
mod promote_scene;

use std::time::Duration;

use kbf_map::{Map, MemberId, MemberState};
use kbf_server::members::{Block, Fill, Handoff, Progress};
use kbf_state::{Cond, Store, Txn};
use promote_scene::{
    SECOND, SETTLE, STALE, flushed_to, handoff, members, node, owned, scene, scene_with, state_of,
};

/// Catches (join test 1): a joiner that is promoted before the farm has flushed what it
/// staged, before the fence, before its own pass, or before the settle time (any of them
/// would make a blob staged before the join read as missing: a promoted joiner is rank 1
/// for part of the digests that exist), and a promotion that loses the maps the old
/// blobs are found through. In order: member 1 still holds a staged blob (it reports it
/// flushed what it staged before the map before the join only): the fence stays shut and the
/// joiner, however it reports, stays `Joining`; then member 1 flushes, the fence opens,
/// the joiner acknowledges the epoch, and it is promoted only after the settle time. At
/// every step every blob of the farm before the join is found through each owner it had,
/// and still is after two more bumps of the map. Catches: no flush barrier (the
/// fence opens at the first sweep); a one-deep previous map (the last assertion); a settle
/// time of zero.
#[test]
fn a_joiner_over_a_farm_with_data_is_promoted_only_when_filled_and_reads_never_miss() {
    let s = scene();
    let joined_at = s.map().generation();
    assert!(s.reads_find_every_old_blob(), "the join itself");

    // Member 1 holds a staged blob: it has flushed only what it staged before the map
    // before the join.
    s.beat(&s.m, &s.a, Progress::CaughtUp, flushed_to(joined_at - 1));
    s.beat(&s.m, &s.b, Progress::CaughtUp, flushed_to(joined_at));
    s.beat_joiner(Progress::CaughtUp, s.done(1));
    s.m.refresh().expect("refresh");
    assert_eq!(s.m.tail_epoch().expect("epoch"), 0, "the fence is shut");
    s.joiner_after(SETTLE * 2, s.done(1));
    assert_eq!(s.state(), Some(MemberState::Joining));
    assert!(matches!(
        s.blocked().first(),
        Some(Block::Flushing { member, have: Some(have), need })
            if *member == s.a.id && *have == joined_at - 1 && *need == joined_at
    ));
    assert!(s.reads_find_every_old_blob(), "while member 1 flushes");

    // It flushes: the sweeper opens the fence.
    s.beat(&s.m, &s.a, Progress::CaughtUp, flushed_to(joined_at));
    s.beat_joiner(Progress::CaughtUp, flushed_to(joined_at));
    s.m.refresh().expect("refresh");
    let epoch = s.m.tail_epoch().expect("epoch");
    assert_eq!(epoch, 1);
    assert_eq!(s.state(), Some(MemberState::Joining));
    assert!(s.reads_find_every_old_blob(), "with the fence open");

    // The joiner's pass is done: the settle time starts, and ends 30 seconds later.
    s.beat_joiner(Progress::CaughtUp, s.done(epoch));
    assert!(matches!(s.blocked().as_slice(), [Block::Settling(_)]));
    s.joiner_after(SETTLE - SECOND, s.done(epoch));
    assert_eq!(s.state(), Some(MemberState::Joining), "one second early");
    s.joiner_after(SECOND, s.done(epoch));
    assert_eq!(s.state(), Some(MemberState::Active));
    assert!(s.blocked().is_empty());
    assert!(owned(&s.map(), s.c.id) > 0);
    assert!(
        s.store.fake.get("unfilled/3").expect("read").is_none(),
        "the mark goes with the promotion"
    );
    assert!(s.reads_find_every_old_blob(), "promoted");

    // Two more bumps of the map (a member restarts twice), then a restart of the frontend.
    for _ in 0..2 {
        s.m.register(&node(1)).expect("restart");
    }
    assert!(s.reads_find_every_old_blob(), "after two more bumps");
    let restarted = members(&s.store, 2, Some(handoff()));
    restarted.current_map().expect("read");
    let (latest, history) = restarted.view().expect("seen");
    let history: Vec<&Map> = history.iter().map(|m| &**m).collect();
    for (digest, owners) in &s.before {
        let order: Vec<MemberId> = latest
            .read_order_with_history(&history, digest)
            .iter()
            .map(|m| m.id)
            .collect();
        assert!(
            owners.iter().all(|o| order.contains(o)),
            "a restarted frontend"
        );
    }
}

/// Catches: the flush barrier counting a member that has not flushed, or one that says it
/// cannot (an older node, one with no gate): the epoch opens, the joiner is promoted, and a
/// blob that member staged is found nowhere but its disk. Each is the only thing wrong.
/// Catches: the generation comparison dropped; a report that cannot say counted as
/// flushed.
#[test]
fn a_member_that_has_not_flushed_or_cannot_say_holds_the_fence_shut() {
    let s = scene();
    let g = s.map().generation();
    for (said, have) in [(flushed_to(g - 1), Some(g - 1)), (Fill::default(), None)] {
        s.beat(&s.m, &s.a, Progress::CaughtUp, said);
        s.beat(&s.m, &s.b, Progress::CaughtUp, flushed_to(g));
        s.beat_joiner(Progress::CaughtUp, flushed_to(g));
        s.m.refresh().expect("refresh");
        assert_eq!(s.m.tail_epoch().expect("epoch"), 0, "{said:?}");
        assert_eq!(
            s.blocked(),
            [Block::Flushing {
                member: s.a.id,
                have,
                need: g
            }]
        );
    }
    s.beat(&s.m, &s.a, Progress::CaughtUp, flushed_to(g));
    s.m.refresh().expect("refresh");
    assert_eq!(s.m.tail_epoch().expect("epoch"), 1);
}

/// Catches: a report counted however old it is: a member that stopped reporting an hour ago
/// (its row lives on) said it had flushed, and the barrier trusts that. Catches: the
/// freshness test dropped.
#[test]
fn a_report_that_is_not_fresh_does_not_count_at_the_barrier() {
    let s = scene();
    let g = s.map().generation();
    s.beat(&s.m, &s.a, Progress::CaughtUp, flushed_to(g));
    s.store.clock.advance(STALE);
    s.beat(&s.m, &s.b, Progress::CaughtUp, flushed_to(g));
    s.beat_joiner(Progress::CaughtUp, flushed_to(g));
    s.m.refresh().expect("refresh");
    assert_eq!(s.m.tail_epoch().expect("epoch"), 0);
    assert_eq!(
        s.blocked(),
        [Block::Flushing {
            member: s.a.id,
            have: None,
            need: g
        }]
    );
    s.beat(&s.m, &s.a, Progress::CaughtUp, flushed_to(g));
    s.m.refresh().expect("refresh");
    assert_eq!(s.m.tail_epoch().expect("epoch"), 1);
}

/// Catches: a joiner promoted while its index rebuild is not done (its beats say it is
/// rebuilding): it would be an owner that can answer neither found nor missing. The beats
/// are otherwise complete and go on for longer than the settle time. Catches: the
/// beat's progress ignored.
#[test]
fn a_joiner_that_is_still_rebuilding_is_not_promoted() {
    let s = scene();
    let epoch = s.open_fence();
    s.beat_joiner(Progress::CatchingUp, s.done(epoch));
    s.store.clock.advance(SETTLE * 2);
    s.beat_joiner(Progress::CatchingUp, s.done(epoch));
    assert_eq!(s.state(), Some(MemberState::Joining));
    s.beat_joiner(Progress::CaughtUp, s.done(epoch));
    s.joiner_after(SETTLE, s.done(epoch));
    assert_eq!(s.state(), Some(MemberState::Active));
}

/// Catches: a joiner promoted without having acknowledged the epoch (it has not adopted the
/// objects other members flushed since its rebuild). It says no pass is done, then the
/// epoch before the fence's (the fence is reopened once, by a change of placement), then
/// the fence's. Catches: the acknowledgement not compared with the epoch.
#[test]
fn a_joiner_that_has_not_acknowledged_the_epoch_is_not_promoted() {
    let s = scene();
    let first = s.open_fence();
    let none = Fill {
        tail_epoch_done: 0,
        ..s.done(first)
    };
    s.beat_joiner(Progress::CaughtUp, none);
    s.joiner_after(SETTLE * 2, none);
    assert_eq!(s.state(), Some(MemberState::Joining));
    assert!(matches!(
        s.blocked().as_slice(),
        [Block::TailPending { done: 0, need }] if *need == first
    ));
    // A fourth member joins and the sweeper reopens the fence: an acknowledgement of the
    // first epoch is one behind.
    members(&s.store, 2, None)
        .register(&node(4))
        .expect("a fourth");
    s.m.refresh().expect("refresh");
    let second = s.m.tail_epoch().expect("epoch");
    assert_eq!(second, first + 1);
    s.beat_joiner(Progress::CaughtUp, s.done(first));
    s.joiner_after(SETTLE * 2, s.done(first));
    assert_eq!(s.state(), Some(MemberState::Joining));
    assert!(matches!(
        s.blocked().as_slice(),
        [Block::TailPending { done, need }] if *done == first && *need == second
    ));
    s.beat_joiner(Progress::CaughtUp, s.done(second));
    s.joiner_after(SETTLE, s.done(second));
    assert_eq!(s.state(), Some(MemberState::Active));
}

/// Catches: an acknowledgement of a boot the fence did not record counted as the joiner's:
/// the node restarted after the fence (its index may be rebuilding) and its new boot says
/// the epoch is done. A frontend that does not advance marks (one without this feature
/// registered the restart) leaves the fence as it was, so the beat alone must refuse. Then
/// the sweeper runs: it opens a newer epoch, records the new boot, the node acknowledges
/// that under its new boot, and the join completes instead of stalling (the "fence boots
/// liveness" of the design). Catches: the boot not compared (promoted at the first
/// settle); `fence_boots` never recorded again (the epoch stays 1 and the final promotion
/// never comes).
#[test]
fn a_restarted_owner_reopens_the_fence_and_then_completes() {
    let s = scene();
    let epoch = s.open_fence();
    let plain = members(&s.store, 2, None);
    let again = plain.register(&node(3)).expect("the joiner restarts");
    assert_eq!(again.boot, s.c.boot + 1);
    // The restarted node claims the epoch at once (a stale or forged acknowledgement).
    let claim = s.done(epoch);
    s.beat(&s.m, &again, Progress::CaughtUp, claim);
    s.store.clock.advance(SETTLE * 2);
    s.beat(&s.m, &again, Progress::CaughtUp, claim);
    assert_eq!(s.state(), Some(MemberState::Joining));
    assert!(matches!(
        s.m.blocked_on(again.id).expect("blocked").as_slice(),
        [Block::Boot { have, fenced: Some(fenced) }] if *have == again.boot && *fenced == s.c.boot
    ));
    assert_eq!(s.m.tail_epoch().expect("epoch"), epoch);

    // The sweeper sees the boot differ: a newer epoch, which the node acknowledges.
    s.m.refresh().expect("refresh");
    let reopened = s.m.tail_epoch().expect("epoch");
    assert_eq!(reopened, epoch + 1);
    s.beat(&s.m, &again, Progress::CaughtUp, claim);
    assert!(matches!(
        s.m.blocked_on(again.id).expect("blocked").as_slice(),
        [Block::TailPending { .. }]
    ));
    let owned_now = Fill {
        tail_epoch_done: reopened,
        ..claim
    };
    s.beat(&s.m, &again, Progress::CaughtUp, owned_now);
    s.store.clock.advance(SETTLE);
    s.beat(&s.m, &again, Progress::CaughtUp, owned_now);
    assert_eq!(s.state(), Some(MemberState::Active));
}

/// Catches: a pass begun under another placement taken as the fill: the joiner's
/// fingerprint (the map it held, itself assumed `Active`) differs from the frontend's, and
/// the epoch is acknowledged. It reports a wrong one for longer than the settle time, then
/// the right one. Catches: the report's fingerprint not compared.
#[test]
fn a_report_made_under_another_placement_is_not_taken_as_the_fill() {
    let s = scene();
    let epoch = s.open_fence();
    let wrong = Fill {
        fill_fp: s.done(epoch).fill_fp ^ 1,
        ..s.done(epoch)
    };
    s.beat_joiner(Progress::CaughtUp, wrong);
    s.joiner_after(SETTLE * 2, wrong);
    assert_eq!(s.state(), Some(MemberState::Joining));
    assert_eq!(s.blocked(), [Block::Fingerprint]);
    s.beat_joiner(Progress::CaughtUp, s.done(epoch));
    s.joiner_after(SETTLE, s.done(epoch));
    assert_eq!(s.state(), Some(MemberState::Active));
}

/// Catches: a fence that outlives a change of placement: a member registered (here by a
/// frontend that does not advance marks, as one mid-rollout) after the epoch was opened, so
/// the joiner's report, made under the new placement and complete, answers an epoch that was
/// opened under the old: the frontend itself must see its mark is stale and open a newer one.
/// The report is withheld until it does; then the joiner acknowledges the newer epoch and is
/// promoted. Catches: the mark's fingerprint not compared (the first settle
/// promotes); a changed fingerprint not re-opening the fence (the epoch stays 1).
#[test]
fn a_change_of_placement_after_the_fence_opens_a_newer_one() {
    let s = scene();
    let epoch = s.open_fence();
    let plain = members(&s.store, 2, None);
    plain.register(&node(4)).expect("a fourth joins");
    let fill = s.done(epoch);
    s.beat_joiner(Progress::CaughtUp, fill);
    s.joiner_after(SETTLE * 2, fill);
    assert_eq!(s.state(), Some(MemberState::Joining), "the mark is stale");
    s.m.refresh().expect("refresh");
    let newer = s.m.tail_epoch().expect("epoch");
    assert_eq!(newer, epoch + 1);
    s.beat_joiner(Progress::CaughtUp, s.done(newer));
    s.joiner_after(SETTLE, s.done(newer));
    assert_eq!(s.state(), Some(MemberState::Active));
}

/// Catches: the settle time counted without the conditions holding: they hold, fail for one
/// beat (a wrong fingerprint), and hold again; the time before the failure must not count.
/// Beats at 0, 20 (failing), 40 (holding: the clock restarts), 69 (29 seconds on: no),
/// 70 (30: yes). Catches: the settle start kept through a failing beat (the beat at
/// 40 would promote).
#[test]
fn the_settle_time_restarts_when_a_condition_fails() {
    let s = scene();
    let epoch = s.open_fence();
    let good = s.done(epoch);
    let wrong = Fill {
        fill_fp: good.fill_fp ^ 1,
        ..good
    };
    s.beat_joiner(Progress::CaughtUp, good);
    s.joiner_after(20 * SECOND, wrong);
    s.joiner_after(20 * SECOND, good);
    assert_eq!(s.state(), Some(MemberState::Joining), "restarted at 40");
    s.joiner_after(29 * SECOND, good);
    assert_eq!(s.state(), Some(MemberState::Joining), "29 seconds on");
    s.joiner_after(SECOND, good);
    assert_eq!(s.state(), Some(MemberState::Active));
}

/// Catches: a settle that does not wait (promotion at the first beat that finds the
/// conditions holding). The conditions hold from the first beat; none promotes before 30
/// seconds on. Catches: the settle time not compared (a zero settle).
#[test]
fn a_joiner_waits_out_the_settle_time() {
    let s = scene();
    let epoch = s.nearly_promoted();
    // The fake store's clock moves a little with every write: no more than a second is left.
    assert!(matches!(
        s.blocked().as_slice(),
        [Block::Settling(left)] if *left <= SECOND && !left.is_zero()
    ));
    s.joiner_after(SECOND, s.done(epoch));
    assert_eq!(s.state(), Some(MemberState::Active));
}

/// Catches: auto-promotion that is not off by default: a frontend without `with_handoff`
/// leaves a joiner whose every condition holds `Joining` for ever, writes no mark body and
/// opens no epoch, and an operator's `active` still promotes it and deletes the mark (the
/// override, and what an old frontend in a mixed rollout does: it never promotes by itself).
/// Catches: `advance` or the beat's step running without the setting.
#[test]
fn without_the_setting_nothing_is_promoted_automatically_and_the_operator_still_can() {
    let s = scene_with(2, None);
    let g = s.map().generation();
    for who in [&s.a, &s.b, &s.c] {
        s.beat(&s.m, who, Progress::CaughtUp, flushed_to(g));
    }
    s.m.refresh().expect("refresh");
    assert_eq!(s.m.tail_epoch().expect("epoch"), 0);
    for _ in 0..4 {
        s.store.clock.advance(SETTLE * 4);
        s.beat_joiner(Progress::CaughtUp, s.done(1));
        s.m.refresh().expect("refresh");
    }
    assert_eq!(s.state(), Some(MemberState::Joining));
    assert_eq!(s.m.tail_epoch().expect("epoch"), 0);
    let mark = s
        .store
        .fake
        .get("unfilled/3")
        .expect("read")
        .expect("a mark");
    assert!(mark.value.is_empty(), "the mark is as it always was");
    assert!(s.m.blocked_joiners().expect("none").is_empty());
    let map =
        s.m.set_state(s.c.id, MemberState::Active)
            .expect("the override");
    assert_eq!(state_of(&map, s.c.id), Some(MemberState::Active));
    assert!(s.store.fake.get("unfilled/3").expect("read").is_none());
}

/// Catches: a frontend that does not have the setting promoting a joiner whose fence another
/// frontend opened (a mixed rollout, or the setting taken off): every condition holds and
/// the settle time has passed, and the beats of the joiner that reach the frontend without
/// the setting promote nothing. The frontend with it promotes at its next beat. Catches:
/// the beat deciding without checking the setting.
#[test]
fn a_frontend_without_the_setting_never_promotes_over_a_fence_another_opened() {
    let s = scene();
    let epoch = s.open_fence();
    let plain = members(&s.store, 2, None);
    s.beat(&plain, &s.c, Progress::CaughtUp, s.done(epoch));
    s.store.clock.advance(SETTLE * 2);
    s.beat(&plain, &s.c, Progress::CaughtUp, s.done(epoch));
    s.store.clock.advance(SETTLE * 2);
    s.beat(&plain, &s.c, Progress::CaughtUp, s.done(epoch));
    assert_eq!(s.state(), Some(MemberState::Joining));
    s.beat_joiner(Progress::CaughtUp, s.done(epoch));
    s.joiner_after(SETTLE, s.done(epoch));
    assert_eq!(s.state(), Some(MemberState::Active));
}

/// Catches: a farm with a member `Down` inside its dwell filling a joiner: that member may
/// hold the only copy of a blob it staged. The fence stays shut while it is down; when it
/// has left the map the fence opens, but the join is degraded and the joiner is withheld for
/// as long as it keeps reporting complete; an operator's `active` still works, and with the
/// degraded setting on the joiner is promoted. Catches: the `Down` member not
/// holding the fence (the epoch opens while it is down); the degraded mark not kept (promoted
/// once it has left); the setting ignored (never promoted with it on).
#[test]
fn a_member_that_went_down_during_the_fill_withholds_promotion_unless_allowed() {
    for allowed in [false, true] {
        let s = scene_with(
            2,
            Some(Handoff {
                settle: SETTLE,
                allow_degraded: allowed,
            }),
        );
        let others = [&s.b, &s.c];
        // Member 1 stops beating: its row lapses and the map says it is Down.
        for _ in 0..3 {
            s.store.clock.advance(Duration::from_secs(1800));
            for who in others {
                s.beat(
                    &s.m,
                    who,
                    Progress::CaughtUp,
                    flushed_to(s.map().generation()),
                );
            }
            s.m.refresh().expect("refresh");
        }
        assert_eq!(state_of(&s.map(), s.a.id), Some(MemberState::Down));
        assert_eq!(
            s.m.tail_epoch().expect("epoch"),
            0,
            "{allowed}: Down holds the fence"
        );
        assert!(s.blocked().contains(&Block::Down(s.a.id)));
        // Its dwell runs out: it leaves the map, and the fence opens.
        for _ in 0..5 {
            s.store.clock.advance(Duration::from_secs(1800));
            for who in others {
                s.beat(
                    &s.m,
                    who,
                    Progress::CaughtUp,
                    flushed_to(s.map().generation()),
                );
            }
            s.m.refresh().expect("refresh");
        }
        assert!(s.map().member(s.a.id).is_none(), "member 1 left");
        let epoch = s.m.tail_epoch().expect("epoch");
        assert!(epoch > 0, "{allowed}: the fence opened without it");
        s.beat_joiner(Progress::CaughtUp, s.done(epoch));
        s.joiner_after(SETTLE * 2, s.done(epoch));
        s.joiner_after(SETTLE * 2, s.done(epoch));
        if allowed {
            assert_eq!(s.state(), Some(MemberState::Active));
        } else {
            assert_eq!(s.state(), Some(MemberState::Joining));
            assert!(s.blocked().contains(&Block::Degraded));
            let map =
                s.m.set_state(s.c.id, MemberState::Active)
                    .expect("override");
            assert_eq!(state_of(&map, s.c.id), Some(MemberState::Active));
        }
    }
}

/// Catches: a change of placement that no member's boot shows (a member goes `Down`: it keeps
/// its slot and its boot) leaving the fence as it was: a report made under the old placement
/// would then be taken for the new. The sweeper must open a newer epoch for the fingerprint
/// alone; the joiner acknowledges it and, the degraded setting being on, is promoted.
/// Catches: the fingerprint left out of the test that reopens the fence.
#[test]
fn a_member_going_down_reopens_the_fence_for_the_new_placement() {
    let s = scene_with(
        2,
        Some(Handoff {
            settle: SETTLE,
            allow_degraded: true,
        }),
    );
    let epoch = s.open_fence();
    let plain = members(&s.store, 2, None);
    for _ in 0..3 {
        s.store.clock.advance(Duration::from_secs(1800));
        for who in [&s.b, &s.c] {
            s.beat(&plain, who, Progress::CaughtUp, s.done(epoch));
        }
        plain.refresh().expect("refresh");
    }
    assert_eq!(state_of(&s.map(), s.a.id), Some(MemberState::Down));
    assert_eq!(
        s.m.tail_epoch().expect("epoch"),
        epoch,
        "nothing reopened it yet"
    );
    s.m.refresh().expect("refresh");
    let newer = s.m.tail_epoch().expect("epoch");
    assert_eq!(newer, epoch + 1);
    s.beat_joiner(Progress::CaughtUp, s.done(newer));
    s.joiner_after(SETTLE, s.done(newer));
    assert_eq!(s.state(), Some(MemberState::Active));
}

/// Catches: a member going `Down` after the fence opened, noticed by a frontend that does not
/// advance marks (so the mark does not say degraded yet), not holding the joiner back: the
/// beat itself must see the `Down` member in the map. Then it returns (registers again,
/// `Active`) after the sweeper marked the join degraded, and the joiner stays withheld: the
/// farm was degraded while it filled. Catches: the degraded mark not kept by the
/// sweeper. (The beat also refuses while a member is `Down`; that is a second guard, since
/// a member going `Down` changes the placement, which holds the beat until the sweeper has
/// reopened the fence and marked the join degraded: no test can tell the guards apart.)
#[test]
fn a_member_down_after_the_fence_withholds_promotion_even_after_it_returns() {
    let s = scene();
    let epoch = s.open_fence();
    let plain = members(&s.store, 2, None);
    for _ in 0..3 {
        s.store.clock.advance(Duration::from_secs(1800));
        for who in [&s.b, &s.c] {
            s.beat(&plain, who, Progress::CaughtUp, s.done(epoch));
        }
        plain.refresh().expect("refresh");
    }
    assert_eq!(state_of(&s.map(), s.a.id), Some(MemberState::Down));
    s.joiner_after(SECOND, s.done(epoch));
    s.joiner_after(SETTLE * 2, s.done(epoch));
    assert_eq!(s.state(), Some(MemberState::Joining));
    assert!(s.blocked().contains(&Block::Down(s.a.id)));
    // The sweeper that advances marks sees it, then the member returns.
    s.m.refresh().expect("refresh");
    s.m.register(&node(1)).expect("the member returns");
    s.m.refresh().expect("refresh");
    assert!(
        s.map()
            .members()
            .iter()
            .all(|m| m.state != MemberState::Down)
    );
    let epoch = s.m.tail_epoch().expect("epoch");
    s.joiner_after(SECOND, s.done(epoch));
    s.joiner_after(SETTLE * 2, s.done(epoch));
    assert_eq!(s.state(), Some(MemberState::Joining));
    assert_eq!(s.blocked(), [Block::Degraded]);
}

/// Catches (r = 1): a fill judged against the map as it is. At one owner per digest a joiner
/// owns nothing while it is `Joining`, so a fingerprint of the map as it is names a placement
/// the promotion then changes: the joiner's report must be of the map with it assumed
/// `Active`. A report of the plain fingerprint is withheld; the assumed one promotes, and
/// the promoted joiner owns digests. Catches: the frontend comparing with the
/// fingerprint of the map as it is.
#[test]
fn at_one_owner_the_fill_is_judged_against_the_map_with_the_joiner_active() {
    let s = scene_with(1, Some(handoff()));
    assert_eq!(owned(&s.map(), s.c.id), 0, "a joiner owns nothing at r = 1");
    let epoch = s.open_fence();
    let plain = Fill {
        fill_fp: s.map().placement_fingerprint(),
        ..s.done(epoch)
    };
    assert_ne!(plain.fill_fp, s.done(epoch).fill_fp);
    s.beat_joiner(Progress::CaughtUp, plain);
    s.joiner_after(SETTLE * 2, plain);
    assert_eq!(s.state(), Some(MemberState::Joining));
    s.beat_joiner(Progress::CaughtUp, s.done(epoch));
    s.joiner_after(SETTLE, s.done(epoch));
    assert_eq!(s.state(), Some(MemberState::Active));
    assert!(owned(&s.map(), s.c.id) > 0);
}

/// Catches: a promotion that overwrites a drain requested between the beat's read and its
/// commit: the joiner's beat that would promote it finds, when it commits, that an operator
/// drained it; the row's version fails the commit, the beat reads `Draining` and promotes
/// nothing, and the mark stays. Catches: the promoting write made without the row's
/// version and without the check on the map (either guard alone holds the commit back: an
/// operator's drain moves both).
#[test]
fn a_drain_requested_during_the_promoting_beat_wins() {
    let s = scene();
    let epoch = s.nearly_promoted();
    let operator = members(&s.store, 2, None);
    let id = s.c.id;
    s.store.after_get("member/3", move || {
        operator
            .set_state(id, MemberState::Draining)
            .expect("drain");
    });
    s.joiner_after(SECOND, s.done(epoch));
    assert_eq!(s.state(), Some(MemberState::Draining));
    assert!(s.store.fake.get("unfilled/3").expect("read").is_some());
}

/// Catches: marks that do not decode, and marks of members the map does not hold, stopping
/// the sweeper (every refresh would then fail to advance any joiner) or promoting by a
/// guess: a garbled mark, a key that is no id and a mark of a member nobody registered are
/// left as they are, and the real joiner is still advanced.
#[test]
fn a_mark_that_does_not_decode_is_left_and_the_others_still_advance() {
    let s = scene();
    let put = |key: &str, value: &[u8]| {
        let txn = Txn::new().put(key, value.to_vec(), Cond::Any);
        assert!(s.store.fake.txn(txn).expect("write").is_ok());
    };
    put("unfilled/x", &[]);
    put("unfilled/99", &[]);
    put("unfilled/2", &[0xff, 0xff, 0xff]);
    s.flush_all(s.map().generation());
    s.m.refresh().expect("refresh");
    assert!(s.m.tail_epoch().expect("epoch") > 0);
    assert!(s.m.blocked_joiners().expect("read").contains_key(&s.c.id));
    assert!(!s.m.blocked_joiners().expect("read").contains_key(&s.b.id));
    // A joiner whose own mark is garbled is promoted by nothing.
    put("unfilled/3", &[0xff, 0xff, 0xff]);
    s.m.refresh().expect("refresh");
    assert!(s.m.blocked_joiners().expect("read").is_empty());
    s.beat_joiner(Progress::CaughtUp, s.done(1));
    s.joiner_after(SETTLE * 2, s.done(1));
    assert_eq!(s.state(), Some(MemberState::Joining));
}

/// Catches: a draining joiner advanced as if it were still filling: its mark is not touched.
#[test]
fn a_joiner_that_is_not_joining_is_not_advanced() {
    let s = scene();
    s.m.set_state(s.c.id, MemberState::Draining).expect("drain");
    s.flush_all(s.map().generation());
    s.m.refresh().expect("refresh");
    assert_eq!(s.m.tail_epoch().expect("epoch"), 0);
    assert!(s.m.blocked_joiners().expect("read").is_empty());
}

/// What an operator is told reads as the sentence it is: each reason names what to look at.
#[test]
fn every_reason_is_said_in_words() {
    let m = |n| MemberId(n);
    let said: Vec<String> = [
        Block::Flushing {
            member: m(1),
            have: Some(3),
            need: 5,
        },
        Block::Flushing {
            member: m(1),
            have: None,
            need: 5,
        },
        Block::Down(m(2)),
        Block::Degraded,
        Block::Rebuilding,
        Block::TailPending { done: 1, need: 2 },
        Block::Boot {
            have: 3,
            fenced: Some(2),
        },
        Block::Boot {
            have: 3,
            fenced: None,
        },
        Block::OwnerBoot {
            member: m(1),
            have: Some(4),
            fenced: Some(3),
        },
        Block::OwnerBoot {
            member: m(1),
            have: Some(4),
            fenced: None,
        },
        Block::OwnerBoot {
            member: m(1),
            have: None,
            fenced: Some(3),
        },
        Block::Fingerprint,
        Block::Settling(Duration::from_secs(7)),
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    for (said, words) in said.iter().zip([
        &["member 1", "map 3", "map 5"][..],
        &["member 1", "not reported", "map 5"],
        &["member 2", "down"],
        &["down", "left"],
        &["rebuild"],
        &["epoch 2", "done 1"],
        &["boot 3", "boot 2"],
        &["boot 3", "after the fence"],
        &["member 1", "boot 4", "recorded boot 3"],
        &["member 1", "registered boot 4", "after the fence"],
        &["member 1", "left the map"],
        &["placement"],
        &["settling", "7s"],
    ]) {
        for word in words {
            assert!(said.contains(word), "{said:?} lacks {word:?}");
        }
    }
}

/// Catches: the fill fields lost between the wire's `from_wire` and the stored row (a
/// generation sent with `flush_known` false read as a generation; a known one read as none).
#[test]
fn the_fill_survives_the_wire_forms() {
    assert_eq!(Fill::from_wire(false, 9, 3, 4).flush_clear_gen, None);
    assert_eq!(Fill::from_wire(true, 9, 3, 4).flush_clear_gen, Some(9));
    assert_eq!(Fill::from_wire(true, 9, 3, 4).flush_to_wire(), (true, 9));
    assert_eq!(Fill::from_wire(false, 9, 3, 4).flush_to_wire(), (false, 0));
    assert_eq!(Fill::default().flush_to_wire(), (false, 0));
    assert_eq!(Handoff::default().settle, Duration::from_secs(30));
    assert!(!Handoff::default().allow_degraded);
}
