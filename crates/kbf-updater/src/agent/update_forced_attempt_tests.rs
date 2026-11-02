//! What a force and an ordinary update report and keep around the edges: when the attempt
//! began, the version said for a release the helper labels otherwise, an older helper's way
//! past another release's trial, and the typed errors of the forced mark.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::hold::{ForcedMark, MarkError, Marks};
use super::local::{LocalError, Trial};
use super::testing::{COMMIT_B, VA, VB, version};
use super::update_forced_tests::{RETRY, Rig, T0, result};
use crate::wire::Outcome;

const OTHER_COMMIT: &str = "cccccccccccccccccccccccccccccccccccccccc";
const OTHER: &str = "0.1.9+gcccccccccccc-4";

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

fn started(agent: &super::run::Agent) -> Option<u64> {
    agent.status().last_update.and_then(|r| r.started_unix)
}

/// Catches: a result that does not say when its attempt began, for the ordinary update and the
/// force alike: both carry the Unix time of the agent's clock at the start of the attempt, and
/// the heartbeat says it.
#[test]
fn an_ordinary_update_and_a_force_report_when_they_began() {
    for (name, forced) in [("started-ordinary", false), ("started-forced", true)] {
        let rig = Rig::new(name);
        rig.coordinator.set_forced(forced);
        rig.keeps_vb();
        let agent = rig.agent();
        rig.unix.store(T0 + 7, Ordering::SeqCst);
        rig.take_order(&agent);
        assert_eq!(started(&agent), Some(T0 + 7), "{name}");
        agent.beat_once();
        let beat = rig.coordinator.beats().pop().unwrap();
        assert_eq!(beat.last_update.unwrap().started_unix, Some(T0 + 7));
    }
}

/// Catches: two attempts at one target that fail the same way being told apart by nothing:
/// each reports the time its own attempt began.
#[test]
fn two_attempts_at_one_target_that_fail_alike_report_different_start_times() {
    for (name, forced) in [
        ("started-twice-ordinary", false),
        ("started-twice-forced", true),
    ] {
        let rig = Rig::new(name);
        rig.coordinator.set_forced(forced);
        rig.keeps_vb();
        rig.local.world().activate = Err(LocalError::Failed("restart_failed".to_owned()));
        rig.local.world().switch = Err(LocalError::Failed("switch_failed".to_owned()));
        let agent = rig.agent();
        rig.take_order(&agent);
        let first = agent.status().last_update.unwrap();
        assert_eq!(first.outcome, Outcome::Failed);
        assert_eq!(first.started_unix, Some(T0));
        rig.clock.advance(Duration::from_secs(RETRY));
        rig.unix.store(T0 + RETRY, Ordering::SeqCst);
        rig.take_order(&agent);
        let second = agent.status().last_update.unwrap();
        assert_eq!(second.outcome, Outcome::Failed);
        assert_eq!(second.started_unix, Some(T0 + RETRY), "{name}");
    }
}

/// Catches: a trial resumed after a restart that keeps the time of the attempt before the
/// restart, or none: the resume is an attempt of its own and begins at the resume.
#[test]
fn a_resumed_trial_reports_the_time_of_the_resume() {
    let rig = Rig::new("started-resume");
    {
        let mut world = rig.local.world();
        world.before = Some(version(VA));
        world.state.version = Some(version(VB));
        world.state.commit = Some(COMMIT_B.to_owned());
        world.state.pending = Some(trial_of(COMMIT_B, VB));
    }
    let agent = rig.agent();
    rig.unix.store(T0 + 500, Ordering::SeqCst);
    let resumed = agent.resumable();
    agent.resume(&resumed);
    assert_eq!(started(&agent), Some(T0 + 500));
}

/// Catches: the result a force publishes before its health wait leaving out when it began.
#[test]
fn the_result_a_force_publishes_early_carries_its_start_time() {
    let rig = Rig::new("started-early");
    rig.keeps_vb();
    let cell: Arc<std::sync::OnceLock<Arc<super::run::Agent>>> = Arc::default();
    let seen = Arc::new(Mutex::new(Vec::new()));
    {
        let (cell, seen) = (Arc::clone(&cell), Arc::clone(&seen));
        *rig.health.on_look.lock().unwrap() = Some(Arc::new(move |kind| {
            if kind == "held" {
                let status = cell.get().and_then(|agent| agent.status().last_update);
                seen.lock()
                    .unwrap()
                    .push(status.and_then(|r| r.started_unix));
            }
        }));
    }
    let agent = Arc::new(rig.agent());
    cell.set(Arc::clone(&agent)).ok();
    rig.take_order(&agent);
    assert_eq!(seen.lock().unwrap()[0], Some(T0));
}

/// Catches: a host that runs the target's commit under another helper label (or none) being
/// reported under that label again after a fast-path success: the agent keeps saying the
/// target, in the status and in the heartbeat, through a refresh and through a duplicate order
/// after an activation. After a restart the agent has no memory of the name and says what
/// the helper says (its label, or the commit-only version when it has none) until the next
/// forced order for the target, which finds the host running it and says the target again.
#[test]
fn a_host_running_the_target_under_another_label_is_reported_as_the_target() {
    let rig = Rig::new("forced-fast-path-name");
    {
        let mut world = rig.local.world();
        world.state.commit = Some(COMMIT_B.to_owned());
        world.state.version = Some(version("9.9"));
    }
    rig.health
        .say("9.9", vec![Ok(crate::ports::Health::Healthy)]);
    let agent = rig.agent();
    assert_eq!(agent.status().version, version("9.9"));
    rig.take_order(&agent);
    assert!(rig.steps().is_empty());
    assert_eq!(agent.status().version, version(VB));
    agent.refresh();
    agent.beat_once();
    assert_eq!(rig.coordinator.beats().pop().unwrap().version, version(VB));

    // After a restart: the helper's word.
    let restarted = rig.agent();
    assert_eq!(restarted.status().version, version("9.9"));
    // A forced order for the target says the target again.
    rig.take_order(&restarted);
    restarted.refresh();
    assert_eq!(restarted.status().version, version(VB));
}

/// Catches: a duplicate forced order for a target that an activation made run (the order is
/// sent again until the coordinator sees the result) clearing the name: it finds the host
/// running the target and keeps saying the target while the helper keeps no label for it.
#[test]
fn a_duplicate_forced_order_after_an_activation_keeps_the_target_as_the_reported_version() {
    let rig = Rig::new("forced-duplicate");
    rig.keeps_vb();
    rig.local.world().activate_label = Some(None);
    let agent = rig.agent();
    rig.take_order(&agent);
    assert_eq!(agent.status().version, version(VB));
    let before = rig.steps().len();
    rig.take_order(&agent);
    assert_eq!(rig.steps().len(), before, "nothing is done twice");
    agent.refresh();
    assert_eq!(agent.status().version, version(VB));
    agent.beat_once();
    assert_eq!(rig.coordinator.beats().pop().unwrap().version, version(VB));
}

/// Catches: a force that fails for a trial of another release on an older helper: the target is
/// kept, so the trial was left for `activate`, which the helper does not know; its `install` is
/// refused while the trial is there. The trial is put back first, then the target is staged,
/// switched and confirmed; the target itself is never reverted.
#[test]
fn an_older_helper_with_another_trial_and_a_kept_target_reverts_that_trial_then_switches() {
    let rig = Rig::new("forced-old-other-trial");
    rig.keeps_vb();
    old_helper(&rig);
    rig.local.world().state.pending = Some(trial_of(OTHER_COMMIT, OTHER));
    let agent = rig.agent();
    rig.take_order(&agent);
    assert_eq!(
        rig.steps(),
        [
            "drain n1 60",
            "local:activate 0.1.7+gbbbbbbbbbbbb-3",
            "local:revert",
            "local:install 0.1.7+gbbbbbbbbbbbb-3 stable",
            "local:switch 0.1.7+gbbbbbbbbbbbb-3",
            "local:confirm 0.1.7+gbbbbbbbbbbbb-3",
            "undrain"
        ]
    );
    assert_eq!(rig.last_update(&agent), result(VB, Outcome::Succeeded));
}

/// Catches: a failed put-back of the other trial going on to an install that is refused, or a
/// trial the helper already ended by itself taken for a failure.
#[test]
fn the_other_trial_that_cannot_be_put_back_fails_the_force_and_one_gone_does_not() {
    for error in [
        LocalError::Failed("revert_failed".to_owned()),
        LocalError::Refused("busy".to_owned()),
    ] {
        let rig = Rig::new("forced-old-revert-fails");
        rig.keeps_vb();
        old_helper(&rig);
        rig.local.world().state.pending = Some(trial_of(OTHER_COMMIT, OTHER));
        rig.local.world().revert = Err(error);
        let agent = rig.agent();
        rig.take_order(&agent);
        assert!(rig.logged("the trial of another release could not be reverted"));
        assert_eq!(rig.events.count("local:install"), 0);
        assert_eq!(rig.last_update(&agent), result(VB, Outcome::Failed));
    }

    let rig = Rig::new("forced-old-revert-gone");
    rig.keeps_vb();
    old_helper(&rig);
    rig.local.world().state.pending = Some(trial_of(OTHER_COMMIT, OTHER));
    rig.local.world().revert = Err(LocalError::Refused("nothing_pending".to_owned()));
    let local = rig.local.clone();
    rig.local.world().on_activate = Some(Arc::new(move || {
        local.world().state.pending = None;
    }));
    let agent = rig.agent();
    rig.take_order(&agent);
    assert_eq!(rig.events.count("local:confirm"), 1);
    assert_eq!(rig.last_update(&agent), result(VB, Outcome::Succeeded));
}

/// Catches: forced-mark errors that are strings: each way a mark cannot be used is a variant
/// of its own that names the file, and a mark that is not there is not an error.
#[test]
fn a_forced_mark_that_cannot_be_used_is_a_typed_error_naming_its_file() {
    let rig = Rig::new("forced-mark-errors");
    let marks = Marks::new(rig.dir.clone());
    assert!(matches!(marks.load_forced("kbf"), Ok(None)));
    let mark = ForcedMark {
        target: version(VB),
    };
    marks.save_forced("kbf", &mark).unwrap();
    assert_eq!(marks.load_forced("kbf").unwrap(), Some(mark));
    let path = rig.dir.join("forced-kbf");

    std::fs::write(&path, vec![b'x'; 5000]).unwrap();
    let err = marks.load_forced("kbf").unwrap_err();
    assert!(
        matches!(&err, MarkError::TooLong { path: p } if *p == path),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        format!("{}: it is too long", path.display())
    );

    std::fs::write(&path, "not json").unwrap();
    let err = marks.load_forced("kbf").unwrap_err();
    assert!(
        matches!(&err, MarkError::Decode { path: p, .. } if *p == path),
        "{err:?}"
    );
    assert!(
        err.to_string()
            .starts_with(&format!("{}: ", path.display()))
    );

    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let err = marks.load_forced("kbf").unwrap_err();
    assert!(
        matches!(&err, MarkError::Io { path: p, .. } if *p == path),
        "{err:?}"
    );
    assert!(
        err.to_string()
            .starts_with(&format!("{}: ", path.display()))
    );
}
