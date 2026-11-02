//! A force against a helper that keeps the contract of the real one: the release it runs
//! under another label or none, an `install` it refuses while a release is on trial, a
//! `switch` that takes only what `install` staged, and one that predates `activate`. Also what
//! the agent publishes and keeps on disk around a force.

use std::sync::{Arc, Mutex, OnceLock};

use super::hold::{ForcedMark, Marks};
use super::local::{Kept, LocalError, Trial};
use super::run::{Agent, coalesce};
use super::testing::{COMMIT_A, COMMIT_B, VA, VB, version};
use super::update_forced_tests::{Rig, T0, not_healthy, result};
use crate::ports::PortError;
use crate::wire::{Health as WireHealth, Outcome};

const OTHER_COMMIT: &str = "cccccccccccccccccccccccccccccccccccccccc";
const OTHER: &str = "0.1.9+gcccccccccccc-4";
const VB_ACTIVATE: &str = "local:activate 0.1.7+gbbbbbbbbbbbb-3";

fn old_helper(rig: &Rig) {
    rig.local.world().activate = Err(LocalError::Refused("bad_command".to_owned()));
}

fn trial_of(commit: &str, text: &str) -> Trial {
    Trial {
        version: Some(version(text)),
        commit: commit.to_owned(),
        deadline_unix: Some(T0 + 600),
    }
}

/// Catches: a result that is published only when the update ends, after the health wait: the
/// coordinator's slot stays taken while the target is looked at. At the first look at the
/// target's health the status already says the target runs, nothing is updating, and the last
/// update is a success; and the same with an older helper's switch and confirm.
#[test]
fn the_result_is_published_before_the_health_is_looked_at() {
    for (name, old) in [("forced-publish", false), ("forced-publish-old", true)] {
        let rig = Rig::new(name);
        rig.keeps_vb();
        if old {
            old_helper(&rig);
        }
        let cell: Arc<OnceLock<Arc<Agent>>> = Arc::default();
        let seen = Arc::new(Mutex::new(Vec::new()));
        {
            let (cell, seen) = (Arc::clone(&cell), Arc::clone(&seen));
            *rig.health.on_look.lock().unwrap() = Some(Arc::new(move |kind| {
                if kind == "held" {
                    seen.lock()
                        .unwrap()
                        .push(cell.get().map(|agent| agent.status()));
                }
            }));
        }
        let agent = Arc::new(rig.agent());
        cell.set(Arc::clone(&agent)).ok();
        rig.take_order(&agent);
        let seen = seen.lock().unwrap();
        let first = seen[0].as_ref().expect("the agent was there");
        assert_eq!(first.version, version(VB), "{name}");
        assert_eq!(first.updating, None, "{name}");
        assert_eq!(first.last_update, result(VB, Outcome::Succeeded), "{name}");
        assert_eq!(first.health, WireHealth::Unhealthy, "not looked at yet");
    }
}

/// Catches: a kept release known by its commit alone, or under another label, whose activation
/// leaves the agent reporting a version the coordinator did not order: the helper keeps its own
/// label for the commit (none, or another), and the status says the ordered version while that
/// commit runs, and the helper's own once another release runs.
#[test]
fn a_forced_release_the_helper_labels_otherwise_is_reported_as_the_target() {
    for (name, listed, label) in [
        ("forced-label-none", None, None),
        (
            "forced-label-other",
            Some("9.9"),
            Some(Some(version("9.9"))),
        ),
    ] {
        let rig = Rig::new(name);
        rig.local.world().state.releases = Some(vec![Kept {
            commit: COMMIT_B.to_owned(),
            version: listed.map(version),
        }]);
        rig.local.world().activate_label = Some(label.unwrap_or(None));
        let agent = rig.agent();
        rig.take_order(&agent);
        assert_eq!(rig.events.count("local:install"), 0, "{name}");
        assert_eq!(rig.events.count("local:activate"), 1, "{name}");
        assert_eq!(agent.status().version, version(VB), "{name}");
        assert_eq!(rig.last_update(&agent), result(VB, Outcome::Succeeded));
        agent.refresh();
        assert_eq!(agent.status().version, version(VB), "after a refresh");

        // Another release runs now (an operator went back by hand): the helper's word again.
        {
            let mut world = rig.local.world();
            world.state.commit = Some(COMMIT_A.to_owned());
            world.state.version = Some(version(VA));
        }
        agent.refresh();
        assert_eq!(agent.status().version, version(VA), "{name}");
    }
}

/// Catches: a host that runs the target under another label, or none, taken for one that does
/// not: it is drained and restarted for nothing. Running is judged by the commit the target's
/// version names.
#[test]
fn a_host_that_runs_the_target_under_another_label_is_done_at_once() {
    let rig = Rig::new("forced-running-label");
    {
        let mut world = rig.local.world();
        world.state.commit = Some(COMMIT_B.to_owned());
        world.state.version = None;
    }
    let agent = rig.agent();
    rig.take_order(&agent);
    assert!(rig.steps().is_empty());
    assert_eq!(rig.last_update(&agent), result(VB, Outcome::Succeeded));
}

/// Catches: an older helper that lists the target but cannot activate: its `switch` takes only
/// a release `install` staged, and a kept one is not (`not_staged`). The force stages it first,
/// then switches and confirms at once; a release staged already by this force is not staged
/// twice.
#[test]
fn an_older_helper_that_lists_the_target_is_asked_to_stage_it_before_the_switch() {
    let rig = Rig::new("forced-old-listed");
    rig.keeps_vb();
    old_helper(&rig);
    let agent = rig.agent();
    rig.take_order(&agent);
    assert_eq!(
        rig.steps(),
        [
            "drain n1 60",
            VB_ACTIVATE,
            "local:install 0.1.7+gbbbbbbbbbbbb-3 stable",
            "local:switch 0.1.7+gbbbbbbbbbbbb-3",
            "local:confirm 0.1.7+gbbbbbbbbbbbb-3",
            "undrain"
        ]
    );
    assert_eq!(rig.last_update(&agent), result(VB, Outcome::Succeeded));
}

/// Catches: an older helper's force that swallows a failed staging: the install fails, nothing
/// is switched, the helper is asked to go back (nothing is pending), and the result is Failed
/// with the step named.
#[test]
fn an_older_helper_whose_staging_fails_fails_the_force() {
    let rig = Rig::new("forced-old-stage-fails");
    rig.keeps_vb();
    old_helper(&rig);
    rig.local.world().install = Err(LocalError::Refused("verify_failed".to_owned()));
    let agent = rig.agent();
    rig.take_order(&agent);
    assert_eq!(rig.events.count("local:switch"), 0);
    assert!(rig.logged("the install that stages the target failed"));
    assert_eq!(rig.last_update(&agent), result(VB, Outcome::Failed));
}

/// Catches: a force that tries to `install` while the helper has the target on trial (it is
/// refused as `pending`), or that reverts the trial it is about to end: the trial of the target
/// is ended by activating it, with no listing needed.
#[test]
fn a_trial_of_the_target_is_ended_by_activating_it() {
    let rig = Rig::new("forced-trial-target");
    {
        let mut world = rig.local.world();
        world.state.version = Some(version(VB));
        world.state.commit = Some(COMMIT_B.to_owned());
        world.state.pending = Some(trial_of(COMMIT_B, VB));
    }
    let agent = rig.agent();
    rig.take_order(&agent);
    assert_eq!(rig.steps(), ["drain n1 60", VB_ACTIVATE, "undrain"]);
    assert!(rig.local.world().state.pending.is_none());
    assert_eq!(rig.last_update(&agent), result(VB, Outcome::Succeeded));
}

/// Catches: a trial of another release that blocks a force for a release the host keeps: the
/// activation ends the trial, so the trial is not reverted first and nothing is fetched.
#[test]
fn a_trial_of_another_release_is_ended_by_activating_a_kept_target() {
    let rig = Rig::new("forced-trial-other-kept");
    rig.keeps_vb();
    rig.local.world().state.pending = Some(trial_of(OTHER_COMMIT, OTHER));
    let agent = rig.agent();
    rig.take_order(&agent);
    assert_eq!(rig.steps(), ["drain n1 60", VB_ACTIVATE, "undrain"]);
    assert!(rig.local.world().state.pending.is_none());
}

/// Catches: a force that installs while another release is on trial (the helper refuses it as
/// `pending`), for a target the listing does not show: no list at all, a list the helper could
/// not read, and a cut list (`releases_omitted`) are all the target unknown. The trial is
/// reverted first, then the target staged and activated.
#[test]
fn a_trial_of_another_release_is_reverted_before_the_target_is_staged() {
    let cut = Some(vec![Kept {
        commit: COMMIT_A.to_owned(),
        version: Some(version(VA)),
    }]);
    for (name, releases) in [
        ("forced-trial-no-list", None),
        ("forced-trial-cut-list", cut),
    ] {
        let rig = Rig::new(name);
        {
            let mut world = rig.local.world();
            world.state.releases = releases;
            world.state.pending = Some(trial_of(OTHER_COMMIT, OTHER));
        }
        let agent = rig.agent();
        rig.take_order(&agent);
        assert_eq!(
            rig.steps(),
            [
                "local:revert",
                "local:install 0.1.7+gbbbbbbbbbbbb-3 stable",
                "drain n1 60",
                VB_ACTIVATE,
                "undrain"
            ],
            "{name}"
        );
        assert_eq!(rig.last_update(&agent), result(VB, Outcome::Succeeded));
    }
}

/// Catches: a force that goes on when the trial it had to put back could not be put back (the
/// install would be refused, and the node would be drained for nothing), and one that takes a
/// helper's `nothing_pending` (it went back by itself) for a failure.
#[test]
fn a_trial_that_cannot_be_reverted_fails_the_force_and_one_already_gone_does_not() {
    for error in [
        LocalError::Failed("revert_failed".to_owned()),
        LocalError::Refused("busy".to_owned()),
    ] {
        let rig = Rig::new("forced-trial-revert-fails");
        rig.local.world().state.pending = Some(trial_of(OTHER_COMMIT, OTHER));
        rig.local.world().revert = Err(error);
        let agent = rig.agent();
        rig.take_order(&agent);
        assert_eq!(rig.steps(), ["local:revert"]);
        assert!(rig.logged("the trial could not be reverted"));
        assert_eq!(rig.last_update(&agent), result(VB, Outcome::Failed));
    }

    let rig = Rig::new("forced-trial-revert-gone");
    rig.local.world().state.pending = Some(trial_of(OTHER_COMMIT, OTHER));
    rig.local.world().revert = Err(LocalError::Refused("nothing_pending".to_owned()));
    // The helper went back by itself in between: nothing is pending for the install.
    let local = rig.local.clone();
    rig.local.world().on_install = Some(Arc::new(move || {
        local.world().state.pending = None;
    }));
    let agent = rig.agent();
    rig.take_order(&agent);
    assert_eq!(rig.events.count("local:activate"), 1);
    assert_eq!(rig.last_update(&agent), result(VB, Outcome::Succeeded));
}

/// Catches: a health port that fails after the activation read as a reason to go back, or to
/// fail: the target stays, the node stays drained, the result is a success, and no revert
/// is asked.
#[test]
fn a_health_check_that_errors_after_the_activation_leaves_the_target_and_the_node_drained() {
    let rig = Rig::new("forced-health-error");
    rig.keeps_vb();
    rig.health
        .say(VB, vec![Err(PortError::other("the check is down"))]);
    let agent = rig.agent();
    rig.take_order(&agent);
    assert_eq!(rig.steps(), ["drain n1 60", VB_ACTIVATE]);
    assert!(rig.drain.is_drained());
    assert!(rig.logged("the check is down"));
    assert_eq!(rig.last_update(&agent), result(VB, Outcome::Succeeded));
    agent.beat_once();
    assert_eq!(
        rig.coordinator.beats().pop().unwrap().health,
        WireHealth::Unhealthy
    );
}

/// Catches: a node that cannot be given back after a healthy forced target turning the force
/// into a failure (the ordinary update's rule, which is for a release on trial): the target
/// runs and stays, the result is a success, the log says the node stays drained, and the
/// heartbeat reports the target healthy.
#[test]
fn an_undrain_that_fails_after_a_healthy_forced_target_is_logged_and_not_a_failure() {
    let rig = Rig::new("forced-undrain-fails");
    rig.keeps_vb();
    *rig.drain.undrain_error.lock().unwrap() = Some(PortError::other("frontend down"));
    let agent = rig.agent();
    rig.take_order(&agent);
    assert_eq!(rig.events.count("local:revert"), 0);
    assert!(rig.drain.is_drained());
    assert!(rig.logged("runs healthy, but the node stays drained"));
    assert_eq!(rig.last_update(&agent), result(VB, Outcome::Succeeded));
    agent.beat_once();
    let beat = rig.coordinator.beats().pop().unwrap();
    assert_eq!(beat.health, WireHealth::Healthy);
    assert_eq!(beat.version, version(VB));
}

/// Catches: a queue of orders that keeps the first order of a service, or the flag of the
/// first: the newest order of a service replaces the queued one whole, forced or not, either
/// way round, and another service's order is queued behind.
#[test]
fn the_newest_order_of_a_service_wins_with_its_flag() {
    let kbf = |target: &str, forced| (None, version(target), forced);
    let mut orders = vec![kbf(VA, true)];
    coalesce(&mut orders, kbf(VB, false));
    assert_eq!(
        orders,
        [kbf(VB, false)],
        "an ordinary order behind a forced one"
    );
    coalesce(&mut orders, kbf(VA, true));
    assert_eq!(
        orders,
        [kbf(VA, true)],
        "a forced order behind an ordinary one"
    );
    let db = (Some("db".to_owned()), version(VB), true);
    coalesce(&mut orders, db.clone());
    coalesce(&mut orders, kbf(VB, false));
    assert_eq!(orders, [kbf(VB, false), db]);
}

/// Catches: a restart between an older helper's `switch` and `confirm` that reverts a force:
/// the agent wrote the force's mark before it began, finds the trial of its target at start,
/// and finishes it as a force: confirm, with no health wait and no revert, though the target
/// is not healthy.
#[test]
fn a_restart_in_an_older_helpers_trial_of_a_force_confirms_it() {
    let rig = Rig::new("forced-restart-trial");
    old_helper(&rig);
    rig.health.say(VB, not_healthy());
    {
        let mut world = rig.local.world();
        world.state.version = Some(version(VB));
        world.state.commit = Some(COMMIT_B.to_owned());
        world.state.pending = Some(trial_of(COMMIT_B, VB));
    }
    let mark = ForcedMark {
        target: version(VB),
    };
    Marks::new(rig.dir.clone())
        .save_forced("kbf", &mark)
        .unwrap();
    let agent = rig.agent();
    let resumed = agent.resumable();
    assert_eq!(resumed, [(0, version(VB), true)]);
    agent.resume(&resumed);
    assert_eq!(
        rig.steps(),
        [
            "drain n1 60",
            VB_ACTIVATE,
            "local:confirm 0.1.7+gbbbbbbbbbbbb-3"
        ]
    );
    assert!(rig.local.world().state.pending.is_none());
    assert_eq!(rig.last_update(&agent), result(VB, Outcome::Succeeded));
    assert!(
        !rig.dir.join("forced-kbf").exists(),
        "the mark is removed at the end"
    );
}

/// Catches: the force's mark making every trial a force: the same restart with no mark is the
/// ordinary resume, which reverts a target that is not healthy.
#[test]
fn a_restart_in_a_trial_with_no_forced_mark_is_resumed_as_an_ordinary_order() {
    let rig = Rig::new("forced-restart-ordinary");
    rig.health.say(VB, not_healthy());
    {
        let mut world = rig.local.world();
        world.before = Some(version(VA));
        world.state.version = Some(version(VB));
        world.state.commit = Some(COMMIT_B.to_owned());
        world.state.pending = Some(trial_of(COMMIT_B, VB));
    }
    let agent = rig.agent();
    let resumed = agent.resumable();
    assert_eq!(resumed, [(0, version(VB), false)]);
    agent.resume(&resumed);
    assert_eq!(rig.events.count("local:revert"), 1);
    assert_eq!(rig.events.count("local:activate"), 0);
}

/// Catches: a mark left by a force that did not reach its end (the agent stopped after
/// `install` and before `activate`) that is read as a force the next time something is on
/// trial: a mark with no trial of its target is removed at start, and the host is still on
/// the release it ran.
#[test]
fn a_mark_that_left_no_trial_is_dropped_at_start() {
    let rig = Rig::new("forced-stale-mark");
    let mark = ForcedMark {
        target: version(VB),
    };
    Marks::new(rig.dir.clone())
        .save_forced("kbf", &mark)
        .unwrap();
    let agent = rig.agent();
    assert_eq!(agent.resumable(), []);
    assert!(!rig.dir.join("forced-kbf").exists());
    assert!(rig.logged("an unfinished forced update is dropped"));
}

/// Catches: a force that persists anything but its mark before the host is switched, or one
/// that has already changed the host between `install` and `activate`: at the moment of
/// `activate` the mark is on disk, the host still runs its old release, and nothing is pending;
/// so an agent stopped there leaves a host the coordinator can order again. The mark is gone
/// when the update ends.
#[test]
fn between_install_and_activate_the_host_is_unchanged_and_the_mark_is_on_disk() {
    let rig = Rig::new("forced-window");
    let seen = Arc::new(Mutex::new(Vec::new()));
    {
        let (local, dir, seen) = (rig.local.clone(), rig.dir.clone(), Arc::clone(&seen));
        rig.local.world().on_activate = Some(Arc::new(move || {
            let state = local.world().state.clone();
            seen.lock().unwrap().push((
                state.version,
                state.commit,
                state.pending.is_none(),
                dir.join("forced-kbf").exists(),
            ));
        }));
    }
    let agent = rig.agent();
    rig.take_order(&agent);
    assert_eq!(
        *seen.lock().unwrap(),
        [(Some(version(VA)), Some(COMMIT_A.to_owned()), true, true)]
    );
    assert_eq!(rig.events.count("local:install"), 1);
    assert!(!rig.dir.join("forced-kbf").exists());
}

/// Catches: a mark that cannot be read at start taken as a force, or one that stops the agent:
/// it is logged and ignored, and the host is resumed as it was found (a directory stands where
/// the mark's file should be).
#[test]
fn a_forced_mark_that_cannot_be_read_is_logged_and_ignored() {
    let rig = Rig::new("forced-mark-unreadable");
    std::fs::create_dir_all(rig.dir.join("forced-kbf/inside")).unwrap();
    let agent = rig.agent();
    assert_eq!(agent.resumable(), []);
    assert!(rig.logged("a forced mark on disk is ignored"));
}

/// Catches: a force that does not run because its mark cannot be written or removed: both are
/// logged, the force goes on, and it ends as a success (a directory stands where the mark's
/// file should be, so it can be neither written nor removed).
#[test]
fn a_force_goes_on_when_its_mark_cannot_be_kept() {
    let rig = Rig::new("forced-mark-unwritable");
    rig.keeps_vb();
    std::fs::create_dir_all(rig.dir.join("forced-kbf/inside")).unwrap();
    let agent = rig.agent();
    rig.take_order(&agent);
    assert!(rig.logged("the forced mark could not be recorded on disk"));
    assert!(rig.logged("the forced mark on disk could not be removed"));
    assert_eq!(rig.events.count("local:activate"), 1);
    assert_eq!(rig.last_update(&agent), result(VB, Outcome::Succeeded));
}

/// Catches: a fake helper looser than the real one, which would let a force that the real
/// helper refuses pass: `install` is refused (`pending`) while a release is on trial, and
/// `switch` takes only a release `install` staged (`not_staged`).
#[test]
fn the_fake_helper_refuses_what_the_real_one_refuses() {
    use super::local::Local as _;
    let rig = Rig::new("forced-fake-contract");
    assert_eq!(
        rig.local.switch(&version(VB)),
        Err(LocalError::Refused("not_staged".to_owned()))
    );
    rig.local.install(&version(VB), "stable").unwrap();
    rig.local.switch(&version(VB)).unwrap();
    assert_eq!(
        rig.local.install(&version(VB), "stable"),
        Err(LocalError::Refused("pending".to_owned()))
    );
}
