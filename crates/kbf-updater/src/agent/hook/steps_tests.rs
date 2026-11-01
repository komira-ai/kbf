//! The hook's steps over the agent's fake ports: the calls they make, step by step, are the
//! calls today's update makes (`agent::update`, pinned by the goldens of `update_tests`), in the
//! same order, and a drain deadline ends `prepare` with exit 75 and the node left drained.

use std::time::Duration;

use update_hook::{Class, Outcome, Phase, Step, Verdict};

use super::testing::{NOW, Rig};
use crate::agent::local::{LocalError, Trial};
use crate::agent::testing::{COMMIT_B, VA, VB, version};
use crate::ports::{Clock, Health, PortError};
use crate::wire;

fn s(text: &str) -> String {
    text.to_owned()
}

fn install() -> String {
    format!("local:install {VB} stable")
}

// ---- the calls of each step ----

/// A normal update, one step at a time: `prepare` asks the helper what runs, has it fetch and
/// verify B, reads the node's placement, drains it and looks at its leases; `cutover` is the
/// helper's switch and nothing else; `verify` is B's health, held drained; `cleanup` confirms B
/// and gives the node back. Each step prints its last line and exits 0.
///
/// Catches: a call moved to another step, or one made twice, or one missing.
#[test]
fn each_step_makes_its_own_calls_and_exits_zero() {
    let rig = Rig::new("hook-steps");
    let mut seen = 0;
    let mut since = |rig: &Rig| {
        let all = rig.calls();
        let new = all[seen..].to_vec();
        seen = all.len();
        new
    };
    let ran = rig.run(Step::Prepare);
    assert_eq!(ran.outcome, Outcome::Success);
    assert_eq!(
        since(&rig),
        [
            s("local:query"),
            install(),
            s("placed"),
            s("drain n1 none"),
            s("leases")
        ]
    );
    assert!(ran.report.reversible);
    let ran = rig.run(Step::Cutover);
    assert_eq!(ran.outcome, Outcome::Success);
    assert_eq!(since(&rig), [format!("local:switch {VB}")]);
    let ran = rig.run(Step::Verify);
    assert_eq!(ran.outcome, Outcome::Success);
    assert_eq!(since(&rig), [format!("health:held {VB}")]);
    assert!(
        ran.report.reversible,
        "verify can be followed by a rollback"
    );
    let ran = rig.run(Step::Cleanup);
    assert_eq!(ran.outcome, Outcome::Success);
    assert_eq!(since(&rig), [format!("local:confirm {VB}"), s("undrain")]);
    assert!(!ran.report.reversible, "the confirm is the commit");
    assert!(!rig.drain.is_drained());
}

/// The order of calls of the whole update, as the updater's own table runs the steps, is the
/// order today's update makes them in: this is the golden of `update_tests`, call for call.
///
/// Catches: any change of order between today's update and the hook.
#[test]
fn the_order_of_calls_of_a_normal_update_is_todays() {
    let rig = Rig::new("hook-order");
    let driven = rig.drive();
    assert_eq!(
        driven.verdict,
        Verdict::Succeeded {
            promotion_pending: false
        }
    );
    assert_eq!(
        driven.steps,
        [
            (Step::Prepare, Outcome::Success),
            (Step::Cutover, Outcome::Success),
            (Step::Verify, Outcome::Success),
            (Step::Cleanup, Outcome::Success),
        ]
    );
    assert_eq!(
        rig.calls(),
        [
            s("local:query"),
            install(),
            s("placed"),
            s("drain n1 none"),
            s("leases"),
            format!("local:switch {VB}"),
            format!("health:held {VB}"),
            format!("local:confirm {VB}"),
            s("undrain"),
        ]
    );
}

/// The same update, and every other path of `update_tests`' goldens, run twice on identical
/// fakes: once as today's update (`agent::update`), once as the hook's steps driven by the
/// updater's table. The calls are the same, in the same order, whether the update succeeds, a
/// health check fails, a confirm is refused, a revert fails or the node cannot be read.
///
/// Catches: the hook drifting from today's sequence on any path, whoever changes either side.
#[test]
fn the_calls_of_the_hook_equal_the_calls_of_todays_update_on_every_path() {
    type Setup = fn(&Rig);
    let paths: Vec<(&str, Setup)> = vec![
        ("a normal update", |_| {}),
        ("leases that drain away", |rig| {
            *rig.drain.leases.lock().unwrap() = [3, 1, 0].into();
        }),
        ("a verify that never turns healthy", |rig| {
            rig.health
                .say(VB, vec![Ok(Health::Waiting(s("not connected")))]);
        }),
        ("an abandoned B and an unhealthy A", |rig| {
            rig.health
                .say(VB, vec![Ok(Health::Abandoned(s("wrong build")))]);
            rig.health
                .say(VA, vec![Ok(Health::Waiting(s("not connected")))]);
        }),
        ("a confirm the helper refuses", |rig| {
            rig.local.world().confirm = Err(LocalError::Failed(s("state_unavailable")));
        }),
        ("a switch the helper cannot do", |rig| {
            rig.local.world().switch = Err(LocalError::Failed(s("switch_failed")));
            rig.local.world().revert = Err(LocalError::Refused(s("nothing_pending")));
        }),
        ("a refused switch and a revert that works", |rig| {
            rig.local.world().switch = Err(LocalError::Refused(s("busy")));
            rig.local.world().before = Some(version(VA));
        }),
        ("a revert that fails", |rig| {
            rig.health
                .say(VB, vec![Ok(Health::Abandoned(s("wrong build")))]);
            rig.local.world().revert = Err(LocalError::Failed(s("revert_failed")));
        }),
        ("a node out of placement", |rig| {
            rig.drain.out_of_placement()
        }),
        ("a fetch the helper refuses", |rig| {
            rig.local.world().install = Err(LocalError::Refused(s("verify_failed")));
        }),
        ("a helper that cannot say what runs", |rig| {
            rig.local.world().query = Err(LocalError::Unreachable(s("no socket")));
        }),
        ("a placement that cannot be read", |rig| {
            *rig.drain.placed_error.lock().unwrap() = Some(PortError::other("503"));
        }),
        ("a drain that fails", |rig| {
            *rig.drain.drain_error.lock().unwrap() = Some(PortError::other("404"));
        }),
        ("leases that cannot be read", |rig| {
            *rig.drain.leases_error.lock().unwrap() = Some(PortError::other("serving again"));
        }),
        ("a release that runs already", |rig| {
            rig.local.world().state.version = Some(version(VB));
        }),
        ("a trial the helper has from before a restart", |rig| {
            {
                let mut world = rig.local.world();
                world.state.version = Some(version(VB));
                world.state.commit = Some(COMMIT_B.to_owned());
                world.state.pending = Some(Trial {
                    version: Some(version(VB)),
                    commit: COMMIT_B.to_owned(),
                    deadline_unix: Some(NOW + 400),
                });
                world.before = Some(version(VA));
            }
            *rig.drain.drained.lock().unwrap() = true;
        }),
    ];
    for (name, setup) in paths {
        let today = Rig::new("hook-equal-today");
        setup(&today);
        let result = today.today();
        let hook = Rig::new("hook-equal-hook");
        setup(&hook);
        let driven = hook.drive();
        assert_eq!(hook.calls(), today.calls(), "{name}");
        assert_eq!(
            matches!(driven.verdict, Verdict::Succeeded { .. }),
            result.outcome == wire::Outcome::Succeeded,
            "{name}: {:?}",
            driven.verdict
        );
    }
}

// ---- the drain deadline ----

/// A drain deadline that passes with a lease still held: `prepare` has looked at the leases
/// every 5 s from 0 to the deadline (13 times for 60 s), has switched nothing, gives back the
/// cordon this update placed, and exits 75 with `retry_after_secs` the retry time (900 s), the
/// reason `drain_bound` and the environment's class (a deferral is about the host's moment).
///
/// Catches: a deadline that exits 1 (the update then counts as failed, not deferred), a node
/// left drained for the updater's rollback to give back, a retry time left out, a deferral
/// counted against the release.
#[test]
fn a_drain_deadline_gives_the_cordon_back_and_exits_75() {
    let mut rig = Rig::new("hook-deadline");
    rig.flags.extend([s("--drain-deadline-secs"), s("60")]);
    *rig.drain.leases.lock().unwrap() = [2].into();
    let ran = rig.run(Step::Prepare);
    assert_eq!(ran.outcome, Outcome::Deferred);
    assert_eq!(ran.report.retry_after_secs, Some(900));
    assert_eq!(ran.reason(), "drain_bound");
    assert!(ran.report.reversible);
    assert_eq!(ran.report.class, Class::Environment);
    assert!(ran.logged("still holds 2 leases after the drain deadline of 60 s"));
    assert!(
        !rig.drain.is_drained(),
        "the cordon this update placed is lifted"
    );
    let mut want = vec![s("local:query"), install(), s("placed"), s("drain n1 60")];
    want.extend(std::iter::repeat_n(s("leases"), 13));
    want.push(s("undrain"));
    assert_eq!(rig.calls(), want);
    let record = rig.record().expect("a record");
    assert_eq!(record.placement, Some(super::record::Was::In));
    assert!(record.drain_begun && record.given_back && !record.prepared);
}

/// The deadline through the updater's table: `prepare` 75, `rollback` 0, the verdict `Deferred`
/// with the retry time, and the node given back by the rollback (the cordon this update
/// placed); nothing was switched, reverted or confirmed.
///
/// Catches: a deferral that leaves the node out of placement until the retry, or reports `Failed`.
#[test]
fn a_drain_deadline_is_a_deferral_and_the_rollback_gives_the_node_back() {
    let mut rig = Rig::new("hook-deadline-table");
    rig.flags.extend([
        s("--drain-deadline-secs"),
        s("60"),
        s("--retry-secs"),
        s("120"),
    ]);
    *rig.drain.leases.lock().unwrap() = [2].into();
    let driven = rig.drive();
    assert_eq!(
        driven.steps,
        [
            (Step::Prepare, Outcome::Deferred),
            (Step::Rollback, Outcome::Success)
        ]
    );
    assert_eq!(
        driven.verdict,
        Verdict::Deferred {
            retry_after_secs: Some(120)
        }
    );
    assert!(!rig.drain.is_drained());
    assert_eq!(rig.events.count("local:switch"), 0);
    assert_eq!(rig.events.count("local:revert"), 0);
    assert_eq!(rig.events.count("undrain"), 1);
    assert!(rig.record().expect("a record").rolled_back);
}

/// With no deadline the leases are waited for however long they take: the drain loop ends when
/// the last lease is gone, and the step exits 0.
///
/// Catches: a default deadline that was never in today's update.
#[test]
fn without_a_deadline_the_leases_are_waited_for_however_long_they_take() {
    let rig = Rig::new("hook-no-deadline");
    *rig.drain.leases.lock().unwrap() = std::iter::repeat_n(4, 500).chain([0]).collect();
    let ran = rig.run(Step::Prepare);
    assert_eq!(ran.outcome, Outcome::Success);
    assert_eq!(rig.events.count("leases"), 501);
    assert_eq!(rig.clock.now(), Duration::from_secs(500 * 5));
}

// ---- the health bound ----

/// `verify` asks B's health every 5 s for the 300 s the timeout allows (61 looks, the first at 0
/// and the last at 300) and exits 1 with the reason and the last thing the check said; the
/// updater's table then runs `rollback`, which reverts, asks what runs, looks at A once and
/// gives the node back.
///
/// Catches: a verify bound other than today's, a verify that reverts by itself, a rollback that
/// skips A's health.
#[test]
fn a_verify_that_never_turns_healthy_fails_at_the_bound_and_the_rollback_goes_back() {
    let rig = Rig::new("hook-unhealthy");
    rig.health
        .say(VB, vec![Ok(Health::Waiting(s("not connected")))]);
    let driven = rig.drive();
    assert_eq!(
        driven.steps,
        [
            (Step::Prepare, Outcome::Success),
            (Step::Cutover, Outcome::Success),
            (Step::Verify, Outcome::Failure),
            (Step::Rollback, Outcome::Success),
        ]
    );
    assert_eq!(
        driven.verdict,
        Verdict::Failed {
            phase: Phase::GateRelease,
            class: Class::Release
        }
    );
    assert_eq!(rig.events.count(&format!("health:held {VB}")), 61);
    assert_eq!(rig.events.count(&format!("health:held {VA}")), 1);
    assert!(!rig.drain.is_drained());
}

/// The wait ends before the helper's own deadline, less the margin: with 100 s left of the
/// helper's window and a 10 s margin B is looked at for 90 s (19 looks), not for the 300 s.
///
/// Catches: a health wait that outlasts the helper's trial, which reverts by itself.
#[test]
fn the_wait_for_health_ends_before_the_helpers_deadline() {
    let rig = Rig::new("hook-window");
    rig.local.world().switch = Ok(NOW + 100);
    rig.health.say(VB, vec![Ok(Health::Waiting(s("not yet")))]);
    assert_eq!(rig.run(Step::Prepare).outcome, Outcome::Success);
    assert_eq!(rig.run(Step::Cutover).outcome, Outcome::Success);
    let ran = rig.run(Step::Verify);
    assert_eq!(ran.outcome, Outcome::Failure);
    assert_eq!(ran.reason(), "not_healthy");
    assert_eq!(rig.events.count(&format!("health:held {VB}")), 19);
}
