//! How each step ends when something is wrong, and what it leaves behind: the reasons and
//! classes of the last line, the node that is given back only when this update drained it, and
//! the steps refusing to run out of order.

use std::sync::Arc;

use update_hook::{Class, Outcome, Phase, Step, Verdict};

use super::record::Was;
use super::testing::Rig;
use crate::agent::cordon::OwnCordon;
use crate::agent::local::{LocalError, Trial};
use crate::agent::testing::{COMMIT_A, COMMIT_B, ServingNode, VA, VB, host, version};
use crate::inventory::Role;
use crate::ports::{Health, Placed, PortError};
use crate::testutil::scratch;

const LEFT_OUT: &str = "node was out of placement before the roll; left out";

fn s(text: &str) -> String {
    text.to_owned()
}

/// The steps up to and including `last` of a successful update, each required to succeed.
fn upto(rig: &Rig, last: Step) {
    for step in [Step::Prepare, Step::Cutover, Step::Verify, Step::Cleanup] {
        assert_eq!(rig.run(step).outcome, Outcome::Success, "{step}");
        if step == last {
            return;
        }
    }
}

/// `rig` has a trial of `VB` on the helper, as after a switch.
fn on_trial(rig: &Rig) {
    let mut world = rig.local.world();
    world.state.version = Some(version(VB));
    world.state.commit = Some(COMMIT_B.to_owned());
    world.state.pending = Some(Trial {
        version: Some(version(VB)),
        commit: COMMIT_B.to_owned(),
        deadline_unix: Some(10_400),
    });
    world.before = Some(version(VA));
}

// ---- prepare ----

/// A health check that refuses (no way to the frontend) stops `prepare` before the helper is
/// asked anything: exit 1, `refused`, the environment's fault, the reason in the log, no record.
///
/// Catches: a precondition read after the helper's query, or taken for the release's fault.
#[test]
fn a_health_refusal_stops_prepare_before_any_call() {
    let rig = Rig::new("hook-refusal");
    *rig.health.refuses.lock().unwrap() = Some(s("no way to the frontend"));
    let ran = rig.run(Step::Prepare);
    assert_eq!(ran.outcome, Outcome::Failure);
    assert_eq!(
        (ran.reason(), ran.report.class),
        ("refused", Class::Environment)
    );
    assert!(ran.logged("no way to the frontend"));
    assert!(rig.calls().is_empty());
    assert_eq!(rig.record(), None);
}

/// The helper that cannot be reached is the environment's fault, a fetch it refuses (attestation,
/// digest, unknown build) is the release's; either way nothing of the node is touched.
///
/// Catches: a release judged bad because a socket was down, or the reverse.
#[test]
fn a_helper_that_fails_stops_prepare_and_the_class_says_whose_fault() {
    for (error, reason, class) in [
        (
            LocalError::Unreachable(s("no socket")),
            "install_failed",
            Class::Environment,
        ),
        (
            LocalError::Refused(s("verify_failed")),
            "install_failed",
            Class::Release,
        ),
        (
            LocalError::Failed(s("install_failed")),
            "install_failed",
            Class::Release,
        ),
    ] {
        let rig = Rig::new("hook-install");
        rig.local.world().install = Err(error.clone());
        let ran = rig.run(Step::Prepare);
        assert_eq!(ran.outcome, Outcome::Failure, "{error:?}");
        assert_eq!(
            (ran.reason(), ran.report.class),
            (reason, class),
            "{error:?}"
        );
        assert_eq!(
            rig.calls().len(),
            2,
            "{error:?}: the query and the install, nothing more"
        );
    }
    let rig = Rig::new("hook-query");
    rig.local.world().query = Err(LocalError::Unreachable(s("no socket")));
    let ran = rig.run(Step::Prepare);
    assert_eq!(
        (ran.reason(), ran.report.class),
        ("helper_unreachable", Class::Environment)
    );
    assert_eq!(rig.calls(), [s("local:query")]);
}

/// A trial on the helper that this update did not start is not finished by it: `prepare` exits
/// 1 with `foreign_trial` and touches nothing (the helper reverts its own trial at its
/// deadline), whether the trial names another version or none.
///
/// Catches: an update that confirms, or drains for, a trial it cannot vouch for.
#[test]
fn a_trial_this_update_did_not_start_is_refused() {
    for trial_version in [Some(version(VA)), None] {
        let rig = Rig::new("hook-foreign");
        rig.local.world().state.pending = Some(Trial {
            version: trial_version,
            commit: COMMIT_B.to_owned(),
            deadline_unix: Some(10_400),
        });
        let ran = rig.run(Step::Prepare);
        assert_eq!(ran.outcome, Outcome::Failure);
        assert_eq!(ran.reason(), "foreign_trial");
        assert_eq!(rig.calls(), [s("local:query")]);
        assert_eq!(rig.record(), None);
    }
}

/// A trial of this update's own version found at `prepare` (the step runner was restarted
/// after the switch) is taken over: the node's placement is read, the deadline kept, and the
/// later steps finish it without fetching, draining or switching again.
///
/// Catches: a second install, drain or switch of an update that is already on trial.
#[test]
fn a_trial_of_this_update_is_taken_over() {
    let rig = Rig::new("hook-adopt");
    on_trial(&rig);
    *rig.drain.drained.lock().unwrap() = true;
    assert_eq!(rig.run(Step::Prepare).outcome, Outcome::Success);
    let record = rig.record().expect("a record");
    assert!(record.switch_begun && record.prepared);
    assert_eq!(record.deadline_unix, Some(10_400));
    assert_eq!(record.placement, Some(Was::In));
    for step in [Step::Cutover, Step::Verify, Step::Cleanup] {
        assert_eq!(rig.run(step).outcome, Outcome::Success, "{step}");
    }
    assert_eq!(
        rig.calls(),
        [
            s("local:query"),
            s("placed"),
            format!("health:held {VB}"),
            format!("local:confirm {VB}"),
            s("undrain"),
        ]
    );
}

/// A trial taken over on a node that cannot be read is not given back: nothing says the cordon
/// is this update's. A placement recorded earlier is used and the node is not read again.
///
/// Catches: a node given back on a guess; the placement read twice.
#[test]
fn a_taken_over_trial_on_an_unreadable_node_is_not_given_back() {
    let rig = Rig::new("hook-adopt-unreadable");
    on_trial(&rig);
    *rig.drain.placed_error.lock().unwrap() = Some(PortError::other("503"));
    let ran = rig.run(Step::Prepare);
    assert_eq!(ran.outcome, Outcome::Success);
    assert!(ran.logged("the node could not be read, so it is not given back"));
    assert_eq!(rig.record().expect("a record").placement, Some(Was::Out));
    upto(&rig, Step::Cleanup);
    assert_eq!(rig.events.count("undrain"), 0);

    let rig = Rig::new("hook-adopt-recorded");
    on_trial(&rig);
    let store = super::record::Store::new(rig.record_dir(), None);
    let mut record = super::record::Record::new(VB, super::testing::DIGEST);
    record.placement = Some(Was::In);
    store.save(&record).unwrap();
    assert_eq!(rig.run(Step::Prepare).outcome, Outcome::Success);
    assert_eq!(
        rig.events.count("placed"),
        0,
        "the recorded placement is used"
    );
    assert_eq!(rig.record().expect("a record").placement, Some(Was::In));
}

/// A host that runs the release already, or that the helper says is current once it has staged
/// it, has nothing to do: every step succeeds, the node is not read, drained or switched, and
/// the commit step still says so.
///
/// Catches: a drain of a node for a release that is already running.
#[test]
fn a_release_that_runs_already_is_done_without_touching_the_node() {
    for (name, staged_current) in [("hook-current-query", false), ("hook-current-staged", true)] {
        let rig = Rig::new(name);
        if staged_current {
            rig.local.world().install = Ok(crate::agent::local::Staged {
                commit: COMMIT_B.to_owned(),
                current: true,
            });
        } else {
            rig.local.world().state.version = Some(version(VB));
        }
        upto(&rig, Step::Cleanup);
        assert!(rig.record().expect("a record").current);
        assert_eq!(rig.events.count("drain"), 0);
        assert_eq!(rig.events.count("placed"), 0);
        assert_eq!(rig.events.count("local:switch"), 0);
        assert_eq!(rig.events.count("local:confirm"), 0);
        assert_eq!(rig.run(Step::Rollback).outcome, Outcome::Success);
        assert_eq!(rig.events.count("local:revert"), 0);
    }
}

/// The artifact `prepare` is given is the staged release of the params, byte for byte; any
/// other is refused before anything else happens.
///
/// Catches: a step run on a directory the updater did not verify.
#[test]
fn prepare_refuses_an_artifact_that_is_not_the_staged_release() {
    let rig = Rig::new("hook-artifact");
    let mut args = rig.args(Step::Prepare);
    let at = args.iter().position(|a| a == "prepare").unwrap() + 1;
    args[at] = s("/somewhere/else");
    let ran = rig.run_args(&args, None);
    assert_eq!(ran.outcome, Outcome::Failure);
    assert_eq!(ran.reason(), "artifact_mismatch");
    assert!(rig.calls().is_empty());
}

/// A drain the frontend refuses leaves the record with the placement and no drain begun, and
/// `rollback` has nothing to give back: no `undrain` is called for a drain that never happened,
/// as in today's update.
///
/// Catches: a node uncordoned after a drain that was refused; a placement lost.
#[test]
fn a_refused_drain_is_not_given_back_and_keeps_the_placement() {
    let rig = Rig::new("hook-drain-refused");
    *rig.drain.drain_error.lock().unwrap() = Some(PortError::other("404"));
    let ran = rig.run(Step::Prepare);
    assert_eq!(ran.outcome, Outcome::Failure);
    assert_eq!(
        (ran.reason(), ran.report.class),
        ("drain_failed", Class::Environment)
    );
    assert!(ran.logged("the node could not be drained: 404"));
    let record = rig.record().expect("a record");
    assert_eq!(
        (record.placement, record.drain_begun),
        (Some(Was::In), false)
    );
    assert_eq!(rig.run(Step::Rollback).outcome, Outcome::Success);
    assert_eq!(rig.events.count("undrain"), 0);
}

/// A placement that cannot be read stops `prepare` before the drain.
///
/// Catches: a drain of a node whose placement is not known.
#[test]
fn a_placement_that_cannot_be_read_stops_before_the_drain() {
    let rig = Rig::new("hook-placement-error");
    *rig.drain.placed_error.lock().unwrap() = Some(PortError::other("503"));
    let ran = rig.run(Step::Prepare);
    assert_eq!(
        (ran.reason(), ran.report.class),
        ("node_unreadable", Class::Environment)
    );
    assert_eq!(rig.events.count("drain n1"), 0);
    assert_eq!(rig.record(), None);
}

/// Leases that cannot be read after the drain stop `prepare` with the node drained; the
/// rollback gives it back, once, and nothing was switched.
///
/// Catches: a node left out of placement for good because the lease read failed.
#[test]
fn leases_that_cannot_be_read_fail_prepare_and_the_rollback_gives_the_node_back() {
    let rig = Rig::new("hook-leases-error");
    *rig.drain.leases_error.lock().unwrap() = Some(PortError::other("serving again"));
    let driven = rig.drive();
    assert_eq!(
        driven.steps,
        [
            (Step::Prepare, Outcome::Failure),
            (Step::Rollback, Outcome::Success)
        ]
    );
    assert_eq!(rig.events.count("undrain"), 1);
    assert_eq!(rig.events.count("local:switch"), 0);
}

/// A `prepare` after the update was rolled back starts a new attempt, from the node's placement
/// on; a record of another update (another release or another artifact) is not this attempt's
/// and is replaced by it.
///
/// Catches: a retry of a deferred update that finds its old record and does nothing, or one that
/// carries the old attempt's placement into the new.
#[test]
fn a_prepare_after_a_rollback_or_after_another_update_starts_afresh() {
    let mut rig = Rig::new("hook-afresh");
    rig.flags.extend([s("--drain-deadline-secs"), s("5")]);
    *rig.drain.leases.lock().unwrap() = [1].into();
    assert_eq!(rig.run(Step::Prepare).outcome, Outcome::Deferred);
    assert_eq!(rig.run(Step::Rollback).outcome, Outcome::Success);
    *rig.drain.leases.lock().unwrap() = [0].into();
    assert_eq!(rig.run(Step::Prepare).outcome, Outcome::Success);
    let record = rig.record().expect("a record");
    assert!(record.prepared && !record.rolled_back);
    assert_eq!(
        rig.events.count("placed"),
        2,
        "the node is read again for the new attempt"
    );

    let rig = Rig::new("hook-other-record");
    let store = super::record::Store::new(rig.record_dir(), None);
    let mut other = super::record::Record::new(VA, super::testing::DIGEST);
    other.prepared = true;
    other.placement = Some(Was::Out);
    store.save(&other).unwrap();
    assert_eq!(rig.run(Step::Prepare).outcome, Outcome::Success);
    let record = rig.record().expect("a record");
    assert_eq!(
        (record.target.as_str(), record.placement),
        (VB, Some(Was::In))
    );
}

// ---- the node is given back only if this update drained it ----

/// A node an operator had drained is drained and waited for like any other and is never given
/// back, on any path: the update that succeeds, the one whose health fails, whose switch fails
/// and whose confirm is refused. The log says it is left out.
///
/// Catches: the incident of an update uncordoning a node an operator drained.
#[test]
fn a_node_out_of_placement_before_the_update_is_never_given_back() {
    type Setup = fn(&Rig);
    let paths: [(&str, Setup); 4] = [
        ("an update that works", |_| {}),
        ("a health check that fails", |rig| {
            rig.health
                .say(VB, vec![Ok(Health::Abandoned(s("wrong build")))]);
        }),
        ("a switch that fails", |rig| {
            rig.local.world().switch = Err(LocalError::Failed(s("switch_failed")));
            rig.local.world().revert = Err(LocalError::Refused(s("nothing_pending")));
            rig.local.world().before = Some(version(VA));
        }),
        ("a confirm that is refused", |rig| {
            rig.local.world().confirm = Err(LocalError::Failed(s("state_unavailable")));
        }),
    ];
    for (name, setup) in paths {
        let rig = Rig::new("hook-operator-drained");
        rig.drain.out_of_placement();
        setup(&rig);
        rig.drive();
        assert_eq!(rig.events.count("undrain"), 0, "{name}");
        assert!(rig.drain.is_drained(), "{name}");
        assert_eq!(
            rig.events.count("drain n1"),
            1,
            "{name}: it is still waited for"
        );
        assert_eq!(
            rig.record().expect("a record").placement,
            Some(Was::Out),
            "{name}"
        );
    }
    let rig = Rig::new("hook-operator-drained-log");
    rig.drain.out_of_placement();
    for step in [Step::Prepare, Step::Cutover, Step::Verify] {
        assert_eq!(rig.run(step).outcome, Outcome::Success);
    }
    assert!(rig.run(Step::Cleanup).logged(LEFT_OUT));
}

/// A node in placement before the update is given back, once: after the confirm by `cleanup`,
/// or by `rollback` on every path that does not end in a confirm, and a second `rollback` or
/// `cleanup` calls `undrain` no more.
///
/// Catches: a node left drained for good; a node uncordoned twice.
#[test]
fn a_node_in_placement_is_given_back_once() {
    let rig = Rig::new("hook-given-back");
    upto(&rig, Step::Cleanup);
    assert_eq!(rig.run(Step::Cleanup).outcome, Outcome::Success);
    assert_eq!(rig.events.count("undrain"), 1);

    let rig = Rig::new("hook-given-back-rollback");
    upto(&rig, Step::Cutover);
    assert_eq!(rig.run(Step::Rollback).outcome, Outcome::Success);
    assert_eq!(rig.run(Step::Rollback).outcome, Outcome::Success);
    assert_eq!(rig.events.count("undrain"), 1);
    assert!(!rig.drain.is_drained());
}

/// The placement is read before the drain and is on disk by then: the record is not empty
/// when the drain call arrives, a `prepare` run again after it finished does not read the node
/// again, and nothing later reads it. (A `prepare` resumed before it finished reads it again:
/// `window_tests::a_crash_between_the_placement_and_the_drain_does_not_hand_the_node_to_the_update`.)
///
/// Catches: a placement read after the drain, which reads the update's own cordon as an
/// operator's (or the reverse), or a record written after the drain.
#[test]
fn the_placement_is_recorded_before_the_drain_and_read_once() {
    let mut rig = Rig::new("hook-before-drain");
    let node = ServingNode::new();
    *node.watch.lock().unwrap() = Some(rig.record_dir());
    rig.drain_port = Arc::new(node.clone());
    assert_eq!(rig.run(Step::Prepare).outcome, Outcome::Success);
    assert_eq!(*node.seen_at_drain.lock().unwrap(), Some(true));
    let record = rig.record().expect("a record");
    assert_eq!(
        (record.placement, record.drain_begun),
        (Some(Was::In), true)
    );

    let rig = Rig::new("hook-read-once");
    upto(&rig, Step::Prepare);
    upto(&rig, Step::Prepare);
    assert_eq!(rig.run(Step::Cutover).outcome, Outcome::Success);
    assert_eq!(rig.run(Step::Rollback).outcome, Outcome::Success);
    assert_eq!(rig.events.count("placed"), 1);
}

/// The update through the agent's own cordon marks (`OwnCordon`) on a node the frontend keeps:
/// a node an operator had drained comes out of the update still drained and with no mark; a
/// serving node comes out serving and with no mark; a node this updater drained on an earlier
/// attempt and never gave back (its mark is on disk, its record gone) is given back.
///
/// Catches: the updater uncordoning an operator's drain; one of its own cordons kept for good.
#[test]
fn through_the_cordon_marks_an_operators_drain_is_kept_and_the_updaters_is_given_back() {
    let marks = |dir: &std::path::Path| {
        std::fs::read_dir(dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("cordon-")
            })
            .count()
    };
    let with_cordon = |name: &str| {
        let mut rig = Rig::new(name);
        let node = ServingNode::new();
        let cordons = scratch(&format!("{name}-marks"));
        rig.drain_port = Arc::new(OwnCordon::new(Arc::new(node.clone()), &cordons, "kbf"));
        (rig, node, cordons)
    };

    let (rig, node, cordons) = with_cordon("hook-cordon-operator");
    node.operator_drains();
    assert_eq!(
        rig.drive().verdict,
        Verdict::Succeeded {
            promotion_pending: false
        }
    );
    assert!(!node.is_serving());
    assert_eq!(marks(&cordons), 0);

    let (rig, node, cordons) = with_cordon("hook-cordon-serving");
    assert_eq!(
        rig.drive().verdict,
        Verdict::Succeeded {
            promotion_pending: false
        }
    );
    assert!(node.is_serving());
    assert_eq!(marks(&cordons), 0);

    // An earlier attempt drained the node, kept its mark, and its record is gone.
    let (rig, node, cordons) = with_cordon("hook-cordon-earlier");
    rig.drain_port
        .drain(&host(Role::Daemon), None)
        .expect("the earlier attempt's drain");
    assert_eq!(marks(&cordons), 1);
    assert!(!node.is_serving());
    assert_eq!(
        rig.drive().verdict,
        Verdict::Succeeded {
            promotion_pending: false
        }
    );
    assert!(
        node.is_serving(),
        "the earlier drain was ours: it is given back"
    );
    assert_eq!(marks(&cordons), 0);
    assert_eq!(rig.record().expect("a record").placement, Some(Was::In));
    assert_eq!(rig.drain_port.placed(&host(Role::Daemon)), Ok(Placed::In));
}

// ---- cutover and verify ----

/// A step that needs `prepare` refuses to run without it, or after the update was rolled back:
/// exit 1, `not_prepared` or `rolled_back`, no call to any port.
///
/// Catches: a switch of a host whose node was never drained.
#[test]
fn a_step_out_of_order_is_refused_without_a_call() {
    let rig = Rig::new("hook-order-refused");
    for step in [Step::Cutover, Step::Verify, Step::Cleanup] {
        let ran = rig.run(step);
        assert_eq!(
            (ran.outcome, ran.reason()),
            (Outcome::Failure, "not_prepared"),
            "{step}"
        );
    }
    // A record whose prepare did not finish.
    let mut slow = Rig::new("hook-order-unfinished");
    slow.flags.extend([s("--drain-deadline-secs"), s("5")]);
    *slow.drain.leases.lock().unwrap() = [1].into();
    assert_eq!(slow.run(Step::Prepare).outcome, Outcome::Deferred);
    assert_eq!(slow.run(Step::Cutover).reason(), "not_prepared");
    assert_eq!(slow.run(Step::Rollback).outcome, Outcome::Success);
    for step in [Step::Cutover, Step::Verify, Step::Cleanup] {
        assert_eq!(slow.run(step).reason(), "rolled_back", "{step}");
    }
    assert_eq!(slow.events.count("local:switch"), 0);
    assert_eq!(slow.events.count("local:confirm"), 0);
}

/// `cleanup` runs only after `verify` succeeded, and `verify` only after `cutover`: a hook that
/// is asked to finish an update that was not verified refuses and changes nothing.
///
/// Catches: a cleanup that commits an update nobody verified.
#[test]
fn cleanup_refuses_to_commit_before_verify_and_verify_before_cutover() {
    let rig = Rig::new("hook-not-verified");
    upto(&rig, Step::Prepare);
    let ran = rig.run(Step::Verify);
    assert_eq!(
        (ran.outcome, ran.reason()),
        (Outcome::Failure, "not_cut_over")
    );
    upto(&rig, Step::Cutover);
    let ran = rig.run(Step::Cleanup);
    assert_eq!(
        (ran.outcome, ran.reason()),
        (Outcome::Failure, "not_verified")
    );
    assert_eq!(rig.events.count("local:confirm"), 0);
}

/// A switch that fails exits 1 (class by whose fault), and the rollback asks the helper to
/// revert, asks what runs, looks at A's health once and gives the node back: the order of
/// today's `back_on_a`. A helper that says nothing is on trial is no failure.
///
/// Catches: a failed switch left drained; a rollback that skips A's health.
#[test]
fn a_switch_that_fails_is_rolled_back_in_todays_order() {
    for (error, class) in [
        (LocalError::Unreachable(s("socket")), Class::Environment),
        (LocalError::Failed(s("switch_failed")), Class::Release),
    ] {
        let rig = Rig::new("hook-switch-failed");
        rig.local.world().switch = Err(error);
        rig.local.world().revert = Err(LocalError::Refused(s("nothing_pending")));
        let driven = rig.drive();
        assert_eq!(
            driven.steps,
            [
                (Step::Prepare, Outcome::Success),
                (Step::Cutover, Outcome::Failure),
                (Step::Rollback, Outcome::Success),
            ]
        );
        assert_eq!(
            driven.verdict,
            Verdict::Failed {
                phase: Phase::BeforeCommit,
                class
            }
        );
        let tail: Vec<String> = rig.calls().into_iter().skip(5).collect();
        assert_eq!(
            tail,
            [
                format!("local:switch {VB}"),
                s("local:revert"),
                s("local:query"),
                format!("health:held {VA}"),
                s("undrain"),
            ]
        );
    }
}

/// A `cutover` run again after it was killed inside the switch: if the helper has the trial it
/// takes its deadline and does not switch a second time; if the helper has none it switches; if
/// the helper cannot be asked it fails with the environment's class.
///
/// Catches: a second switch over a trial already on; a trial never switched taken for done.
#[test]
fn a_cutover_run_again_looks_at_the_helper_before_it_switches() {
    let marked = |name: &str| {
        let rig = Rig::new(name);
        upto(&rig, Step::Prepare);
        let store = super::record::Store::new(rig.record_dir(), None);
        let mut record = rig.record().expect("a record");
        record.switch_begun = true;
        store.save(&record).unwrap();
        rig
    };
    let rig = marked("hook-cutover-trial");
    on_trial(&rig);
    assert_eq!(rig.run(Step::Cutover).outcome, Outcome::Success);
    assert_eq!(rig.events.count("local:switch"), 0);
    assert_eq!(rig.record().expect("a record").deadline_unix, Some(10_400));

    let rig = marked("hook-cutover-no-trial");
    assert_eq!(rig.run(Step::Cutover).outcome, Outcome::Success);
    assert_eq!(rig.events.count("local:switch"), 1);
    assert_eq!(rig.record().expect("a record").deadline_unix, Some(10_600));

    let rig = marked("hook-cutover-other-trial");
    rig.local.world().state.pending = Some(Trial {
        version: Some(version(VA)),
        commit: COMMIT_A.to_owned(),
        deadline_unix: Some(10_400),
    });
    assert_eq!(rig.run(Step::Cutover).outcome, Outcome::Success);
    assert_eq!(
        rig.events.count("local:switch"),
        1,
        "a trial of another version is not ours"
    );

    let rig = marked("hook-cutover-mute");
    rig.local.world().query = Err(LocalError::Unreachable(s("socket")));
    let ran = rig.run(Step::Cutover);
    assert_eq!(
        (ran.reason(), ran.report.class),
        ("helper_unreachable", Class::Environment)
    );
    assert_eq!(rig.events.count("local:switch"), 0);
}

/// `verify` ends with the reason the check gave: a release that will not get there (abandoned)
/// and one that is not yet there at the bound are told apart, and both are the release's.
///
/// Catches: a verify failure with no reason; the two taken for each other.
#[test]
fn verify_names_why_the_release_is_not_healthy() {
    let rig = Rig::new("hook-verify-abandoned");
    upto(&rig, Step::Cutover);
    rig.health
        .say(VB, vec![Ok(Health::Abandoned(s("wrong build")))]);
    let ran = rig.run(Step::Verify);
    assert_eq!((ran.outcome, ran.reason()), (Outcome::Failure, "abandoned"));
    assert_eq!(ran.report.detail.as_deref(), Some("wrong build"));
    assert!(ran.logged("it will not get there: wrong build"));
    assert_eq!(rig.events.count(&format!("health:held {VB}")), 1);

    let rig = Rig::new("hook-verify-waiting");
    upto(&rig, Step::Cutover);
    rig.health.say(
        VB,
        vec![
            Err(PortError::other("503")),
            Ok(Health::Waiting(s("not yet"))),
        ],
    );
    let ran = rig.run(Step::Verify);
    assert_eq!(
        (ran.outcome, ran.reason()),
        (Outcome::Failure, "not_healthy")
    );
    assert!(ran.logged("not healthy after the time allowed: not yet"));
}

// ---- rollback ----

/// A `rollback` of an update that has no record, or whose record is another update's, has
/// nothing to undo: exit 0, no call.
///
/// Catches: a rollback that reverts a release it never switched.
#[test]
fn a_rollback_with_nothing_of_this_update_does_nothing() {
    let rig = Rig::new("hook-rollback-nothing");
    assert_eq!(rig.run(Step::Rollback).outcome, Outcome::Success);
    let store = super::record::Store::new(rig.record_dir(), None);
    let mut other = super::record::Record::new(VA, super::testing::DIGEST);
    other.switch_begun = true;
    store.save(&other).unwrap();
    assert_eq!(rig.run(Step::Rollback).outcome, Outcome::Success);
    assert!(rig.calls().is_empty());
    assert_eq!(
        rig.record(),
        Some(other),
        "another update's record is not touched"
    );
}

/// Once the helper has confirmed, the release cannot go back: `rollback` exits 1 with
/// `reversible: false` and the reason `committed`, and calls nothing. A confirm that was begun
/// and not recorded is looked up at the helper: confirmed there means committed, still on trial
/// means reverted.
///
/// Catches: a rollback of a committed update; a killed confirm taken for either answer.
#[test]
fn a_rollback_after_the_commit_says_it_cannot_and_changes_nothing() {
    let rig = Rig::new("hook-rollback-committed");
    upto(&rig, Step::Cleanup);
    let calls = rig.calls().len();
    let ran = rig.run(Step::Rollback);
    assert_eq!(
        (ran.outcome, ran.reason(), ran.report.reversible),
        (Outcome::Failure, "committed", false)
    );
    assert_eq!(rig.calls().len(), calls);
    let driven_verdict = update_hook::next(
        Step::Rollback,
        ran.outcome,
        &ran.report,
        &update_hook::Context::default(),
    );
    assert_eq!(
        driven_verdict,
        update_hook::Next::Report(Verdict::Failed {
            phase: Phase::AfterCommit,
            class: Class::Release
        })
    );

    let begun = |name: &str, confirmed_at_helper: bool| {
        let rig = Rig::new(name);
        upto(&rig, Step::Verify);
        let store = super::record::Store::new(rig.record_dir(), None);
        let mut record = rig.record().expect("a record");
        record.confirm_begun = true;
        store.save(&record).unwrap();
        if confirmed_at_helper {
            rig.local.world().state.pending = None;
        }
        rig
    };
    let rig = begun("hook-rollback-confirmed", true);
    assert_eq!(rig.run(Step::Rollback).reason(), "committed");
    assert_eq!(rig.events.count("local:revert"), 0);
    let rig = begun("hook-rollback-unconfirmed", false);
    assert_eq!(rig.run(Step::Rollback).outcome, Outcome::Success);
    assert_eq!(rig.events.count("local:revert"), 1);
    let rig = begun("hook-rollback-mute", false);
    rig.local.world().query = Err(LocalError::Unreachable(s("socket")));
    let ran = rig.run(Step::Rollback);
    assert_eq!(
        (ran.reason(), ran.report.class),
        ("helper_unreachable", Class::Environment)
    );
}

/// A revert the helper fails is told: exit 1, `revert_failed`, and the record stays open so the
/// next `rollback` tries again; a revert of nothing pending is no failure. The old release is
/// looked at all the same.
///
/// Catches: a rollback that says it worked when the old release did not come back.
#[test]
fn a_revert_that_fails_fails_the_rollback_and_it_can_be_tried_again() {
    let rig = Rig::new("hook-revert-fails");
    upto(&rig, Step::Cutover);
    rig.local.world().revert = Err(LocalError::Failed(s("revert_failed")));
    let ran = rig.run(Step::Rollback);
    assert_eq!(
        (ran.outcome, ran.reason()),
        (Outcome::Failure, "revert_failed")
    );
    assert!(ran.logged("the revert failed"));
    assert!(!rig.record().expect("a record").rolled_back);
    rig.local.world().revert = Err(LocalError::Refused(s("busy")));
    let ran = rig.run(Step::Rollback);
    assert_eq!(
        (ran.outcome, ran.reason()),
        (Outcome::Failure, "revert_failed")
    );
    rig.local.world().revert = Ok(());
    assert_eq!(rig.run(Step::Rollback).outcome, Outcome::Success);
    assert!(rig.record().expect("a record").rolled_back);
}

/// An old release that is not healthy, or that the host does not name, keeps the node drained:
/// `rollback` exits 1 with `a_unhealthy` and does not call `undrain`. A host that names its
/// release only by commit is looked at under the version that commit gives.
///
/// Catches: a node given back onto a release that is not serving.
#[test]
fn a_node_is_not_given_back_onto_an_old_release_that_is_not_healthy() {
    let rig = Rig::new("hook-a-unhealthy");
    upto(&rig, Step::Cutover);
    rig.health
        .say(VA, vec![Ok(Health::Waiting(s("not connected")))]);
    let ran = rig.run(Step::Rollback);
    assert_eq!(
        (ran.outcome, ran.reason()),
        (Outcome::Failure, "a_unhealthy")
    );
    assert!(ran.logged("A is not healthy: the node stays drained"));
    assert_eq!(rig.events.count("undrain"), 0);
    assert!(rig.drain.is_drained());
    assert_eq!(rig.events.count(&format!("health:held {VA}")), 61);

    let rig = Rig::new("hook-a-unknown");
    upto(&rig, Step::Cutover);
    rig.local.world().revert = Err(LocalError::Refused(s("nothing_pending")));
    {
        let mut world = rig.local.world();
        world.state.version = None;
        world.state.commit = Some(COMMIT_A.to_owned());
    }
    rig.health
        .say("0.1.0+gaaaaaaaaaaaa", vec![Ok(Health::Healthy)]);
    assert_eq!(rig.run(Step::Rollback).outcome, Outcome::Success);
    assert!(rig.calls().contains(&s("health:held 0.1.0+gaaaaaaaaaaaa")));

    // A host that names neither a version nor a commit.
    let rig = Rig::new("hook-a-nameless");
    upto(&rig, Step::Cutover);
    rig.local.world().revert = Err(LocalError::Refused(s("nothing_pending")));
    {
        let mut world = rig.local.world();
        world.state.version = None;
        world.state.commit = None;
    }
    assert_eq!(rig.run(Step::Rollback).reason(), "a_unhealthy");

    let rig = Rig::new("hook-a-mute");
    upto(&rig, Step::Cutover);
    rig.local.world().mute_after_revert = true;
    let ran = rig.run(Step::Rollback);
    assert_eq!(ran.reason(), "a_unhealthy");
    assert!(ran.logged("the host does not say what runs"));
}

/// An `undrain` the frontend refuses fails the rollback with `undrain_failed` (the environment's
/// fault), keeps the node marked as not given back, and the next `rollback` gives it back.
///
/// Catches: a node recorded as given back that never was.
#[test]
fn an_undrain_that_fails_is_tried_again_by_the_next_rollback() {
    let rig = Rig::new("hook-undrain-fails");
    upto(&rig, Step::Cutover);
    *rig.drain.undrain_error.lock().unwrap() = Some(PortError::other("503"));
    let ran = rig.run(Step::Rollback);
    assert_eq!(
        (ran.outcome, ran.reason(), ran.report.class),
        (Outcome::Failure, "undrain_failed", Class::Environment)
    );
    assert!(ran.logged("the node stays drained: 503"));
    assert!(!rig.record().expect("a record").given_back);
    *rig.drain.undrain_error.lock().unwrap() = None;
    assert_eq!(rig.run(Step::Rollback).outcome, Outcome::Success);
    assert!(!rig.drain.is_drained());
}

// ---- cleanup ----

/// A confirm the helper refuses is reverted like any other release that did not work, in
/// today's order (confirm, revert, query, A's health, undrain), the step exits 1 with
/// `confirm_refused`, and the update is over: `cleanup` run again refuses, and the record says
/// rolled back.
///
/// Catches: a refused confirm that leaves B on trial until the helper's deadline.
#[test]
fn a_refused_confirm_is_reverted_by_the_step_that_made_it() {
    let rig = Rig::new("hook-confirm-refused");
    upto(&rig, Step::Verify);
    rig.local.world().confirm = Err(LocalError::Failed(s("state_unavailable")));
    let ran = rig.run(Step::Cleanup);
    assert_eq!(
        (ran.outcome, ran.reason()),
        (Outcome::Failure, "confirm_refused")
    );
    assert!(ran.report.reversible, "nothing was committed");
    let tail: Vec<String> = rig.calls().into_iter().skip(7).collect();
    assert_eq!(
        tail,
        [
            format!("local:confirm {VB}"),
            s("local:revert"),
            s("local:query"),
            format!("health:held {VA}"),
            s("undrain"),
        ]
    );
    let record = rig.record().expect("a record");
    assert!(record.rolled_back && !record.confirm_begun && !record.confirmed);
    assert_eq!(rig.run(Step::Cleanup).reason(), "rolled_back");
    // The updater runs `rollback` after a cleanup that says it is reversible: the revert is
    // done, so nothing is called twice and the step succeeds.
    let calls = rig.calls().len();
    assert_eq!(rig.run(Step::Rollback).outcome, Outcome::Success);
    assert_eq!(rig.calls().len(), calls);
    assert_eq!(rig.events.count("local:revert"), 1);
    assert_eq!(rig.events.count("undrain"), 1);

    // The revert itself fails: the update is not over, and the next try goes the same road.
    let rig = Rig::new("hook-confirm-refused-revert-fails");
    upto(&rig, Step::Verify);
    rig.local.world().confirm = Err(LocalError::Failed(s("state_unavailable")));
    rig.local.world().revert = Err(LocalError::Failed(s("revert_failed")));
    let ran = rig.run(Step::Cleanup);
    assert_eq!(
        (ran.reason(), ran.report.class),
        ("confirm_refused", Class::Release)
    );
    assert!(
        ran.report.reversible,
        "the updater's rollback goes on from here"
    );
    assert!(!rig.record().expect("a record").rolled_back);
}

/// An `undrain` that fails after the confirm fails `cleanup` with the update committed and the
/// node drained; the next `cleanup` does not confirm again and gives the node back.
///
/// Catches: a second confirm; a committed update with its node out of placement for good.
#[test]
fn an_undrain_that_fails_after_the_confirm_is_the_only_thing_tried_again() {
    let rig = Rig::new("hook-cleanup-undrain");
    upto(&rig, Step::Verify);
    *rig.drain.undrain_error.lock().unwrap() = Some(PortError::other("503"));
    let ran = rig.run(Step::Cleanup);
    assert_eq!(
        (ran.outcome, ran.reason()),
        (Outcome::Failure, "undrain_failed")
    );
    assert!(
        !ran.report.reversible,
        "after the commit a failure is not reversible"
    );
    assert!(rig.record().expect("a record").confirmed);
    *rig.drain.undrain_error.lock().unwrap() = None;
    let ran = rig.run(Step::Cleanup);
    assert_eq!(ran.outcome, Outcome::Success);
    assert!(!ran.report.reversible);
    assert_eq!(rig.events.count("local:confirm"), 1);
    assert!(!rig.drain.is_drained());
}

/// A `cleanup` run again after it was killed inside the confirm: if the helper has confirmed
/// (no trial, B running) it does not confirm again; if it has not, it confirms; if the helper
/// cannot be asked it fails with the environment's class.
///
/// Catches: a second confirm of a confirmed release; a confirm skipped on a guess.
#[test]
fn a_cleanup_run_again_looks_at_the_helper_before_it_confirms() {
    let begun = |name: &str| {
        let rig = Rig::new(name);
        upto(&rig, Step::Verify);
        let store = super::record::Store::new(rig.record_dir(), None);
        let mut record = rig.record().expect("a record");
        record.confirm_begun = true;
        store.save(&record).unwrap();
        rig
    };
    let rig = begun("hook-cleanup-confirmed");
    rig.local.world().state.pending = None;
    assert_eq!(rig.run(Step::Cleanup).outcome, Outcome::Success);
    assert_eq!(rig.events.count("local:confirm"), 0);

    let rig = begun("hook-cleanup-unconfirmed");
    assert_eq!(rig.run(Step::Cleanup).outcome, Outcome::Success);
    assert_eq!(rig.events.count("local:confirm"), 1);

    let rig = begun("hook-cleanup-mute");
    rig.local.world().query = Err(LocalError::Unreachable(s("socket")));
    let ran = rig.run(Step::Cleanup);
    assert_eq!(
        (ran.reason(), ran.report.class),
        ("helper_unreachable", Class::Environment)
    );
    assert_eq!(rig.events.count("local:confirm"), 0);
}
