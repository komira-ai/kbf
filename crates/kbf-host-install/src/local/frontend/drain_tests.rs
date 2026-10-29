//! The drain of a frontend switch over a fake runtime: it is waited for however long it takes
//! unless a deadline is configured, a deadline that passes stops the switch without cutting
//! anything, and the trial window starts after the new container does.

use super::config::FrontendConfig;
use super::fake::Source;
use super::tests::{CONFIG, DRAIN, NEW, NEW_ID, OLD, OLD_ID, Rig, WINDOW, failed, refused};
use crate::local::answer::{Answer, Code};
use crate::local::backend::Backend;

/// With no `drain_secs` the drain is waited for however long it takes: the clock goes far
/// past any deadline an older helper had (180 s, 1800 s), and the switch goes on once the
/// container has ended.
#[test]
fn with_no_drain_deadline_the_switch_waits_until_the_container_ends() {
    let rig = Rig::new("drain-no-deadline");
    rig.world.0.borrow_mut().ends_after = Some(5000);
    assert!(matches!(rig.update(), Answer::Switched { .. }));
    assert_eq!(rig.world.0.borrow().drain_looks, 5000);
    assert!(rig.world.elapsed().as_secs() >= 5000);
    let steps = rig.steps();
    let position = |what: &str| steps.iter().position(|e| e == what).unwrap();
    assert!(position(DRAIN) < position("stop"));
    assert_eq!(rig.world.made().unwrap().image_id, NEW_ID);
}

/// A configured deadline only stops the waiting: the switch fails, the runtime saw no stop, no
/// remove and no run, the old container still runs (draining) with its configured restart
/// policy, and nothing is on trial. When the drain finishes, asking again switches.
#[test]
fn a_drain_deadline_that_passes_stops_the_switch_and_cuts_nothing() {
    let config = CONFIG.replace(
        "api =",
        "drain_secs = 20
ready_secs = 30
api =",
    );
    let rig = Rig::with_config("drain-deadline", &config);
    rig.world.0.borrow_mut().never_ends = true;
    let answer = rig.update();
    assert!(failed(Code::SwitchFailed)(&answer), "{answer:?}");
    assert!(
        matches!(&answer, Answer::Failed(_, why) if why.contains("still draining")),
        "{answer:?}"
    );
    assert!(rig.world.elapsed().as_secs() >= 20);
    assert_eq!(
        rig.steps(),
        [
            "fetch".to_owned(),
            "load".to_owned(),
            "set_restart:no".to_owned(),
            DRAIN.to_owned(),
            "set_restart:unless-stopped".to_owned(),
        ]
    );
    let made = rig.world.made().unwrap();
    assert_eq!((made.image_id.as_str(), made.running), (OLD_ID, true));
    assert!(made.draining);
    assert_eq!(made.restart, "unless-stopped");
    assert!(rig.record().pending.is_none());
    let view = rig.query();
    assert_eq!(view.commit.as_deref(), Some(OLD));
    assert!(view.pending.is_none());

    // The work ends; the same request now goes through.
    rig.world.0.borrow_mut().never_ends = false;
    assert!(matches!(rig.switch(), Answer::Switched { .. }));
    assert_eq!(rig.world.made().unwrap().image_id, NEW_ID);
}

/// The trial window counts from the new container's start: a long drain neither shortens it
/// nor lets the deadline revert pass before the release can be confirmed.
#[test]
fn the_trial_deadline_is_counted_from_after_the_new_container_starts() {
    let rig = Rig::new("window-after-drain");
    rig.world.0.borrow_mut().ends_after = Some(3000);
    let Answer::Switched { deadline_unix, .. } = rig.update() else {
        panic!("not switched");
    };
    let drained = rig.world.elapsed().as_secs();
    assert!(drained >= 3000);
    // The clock of the answer is 1000 when the request came; the window is from the start.
    assert_eq!(deadline_unix, 1000 + drained + WINDOW);
    assert_eq!(
        rig.record().pending.unwrap().deadline_unix,
        Some(deadline_unix)
    );
    // The real time has gone on by the drain: just before the deadline nothing reverts, and
    // at the deadline the release on trial is put back.
    let backend = Rig::process(
        &rig.dir,
        &rig.world,
        Source::new(&rig.world, NEW),
        &rig.clock,
    );
    rig.clock.set(deadline_unix - 1);
    assert!(backend.tick().is_none());
    rig.clock.set(deadline_unix);
    assert!(matches!(backend.tick(), Some(Answer::Reverted { .. })));
}

/// The window holds the new container's wait for readiness and nothing of the drain: a long
/// `drain_secs` fits any window, one shorter than `ready_secs` is refused before anything is
/// written.
#[test]
fn a_window_must_hold_the_readiness_wait_but_not_the_drain() {
    let config = CONFIG.replace(
        "api =",
        "drain_secs = 7200
api =",
    );
    let rig = Rig::with_config("window", &config);
    assert!(matches!(rig.install(), Answer::Staged { .. }));
    let fault = rig.backend().switch(NEW, 60, 1000).unwrap_err();
    assert!(
        fault.refused && fault.code == Code::ConfigInvalid,
        "{fault:?}"
    );
    assert!(rig.record().pending.is_none());
    assert_eq!(rig.world.made().unwrap().image_id, OLD_ID);
    // The same configuration with the full window goes through.
    assert!(matches!(rig.switch(), Answer::Switched { .. }));
}

/// A drain request that fails, and a runtime that stops answering during the wait, both leave
/// the old container running as it was, with its configured restart policy back, and nothing
/// stopped, removed or recorded.
#[test]
fn a_failed_drain_leaves_the_old_container_running_with_its_policy() {
    let steps = [
        "fetch".to_owned(),
        "load".to_owned(),
        "set_restart:no".to_owned(),
        DRAIN.to_owned(),
        "set_restart:unless-stopped".to_owned(),
    ];
    let rig = Rig::new("drain-exec-fails");
    rig.world.fail("exec");
    assert!(failed(Code::SwitchFailed)(&rig.update()));
    assert_eq!(rig.steps(), steps);
    let made = rig.world.made().unwrap();
    assert_eq!((made.image_id.as_str(), made.running), (OLD_ID, true));
    assert_eq!(made.restart, "unless-stopped");
    assert!(rig.record().pending.is_none());

    let rig = Rig::new("drain-inspect-dies");
    assert!(matches!(rig.install(), Answer::Staged { .. }));
    {
        let mut world = rig.world.0.borrow_mut();
        world.inspects = 0;
        world.inspect_fails_from = Some(3);
        world.never_ends = true;
    }
    assert!(failed(Code::SwitchFailed)(&rig.switch()));
    assert_eq!(rig.steps(), steps);
    let made = rig.world.made().unwrap();
    assert_eq!((made.image_id.as_str(), made.running), (OLD_ID, true));
    assert_eq!(made.restart, "unless-stopped");
    assert!(rig.record().pending.is_none());
}

/// A file with `drain_secs = 0`, which older programs accepted, does not stop the program from
/// starting; the switch refuses it, naming the key, and touches nothing.
#[test]
fn a_zero_drain_secs_is_refused_by_the_switch_not_at_start() {
    let rig = Rig::new("drain-zero");
    assert!(matches!(rig.install(), Answer::Staged { .. }));
    // The file is edited after the release was laid out.
    super::tests::put(
        &rig.config_path(),
        &CONFIG.replace("api =", "drain_secs = 0\napi ="),
    );
    assert!(refused(Code::ConfigInvalid)(&rig.switch()));
    let fault = rig.backend().switch(NEW, WINDOW, 1000).unwrap_err();
    assert!(fault.detail.contains("drain_secs"), "{fault:?}");
    assert_eq!(rig.world.made().unwrap().image_id, OLD_ID);
    assert!(!rig.steps().contains(&"set_restart:no".to_owned()));
    // The start-up reading of the same file accepts it.
    let trust = rig.backend().host.trust.clone();
    let loaded = FrontendConfig::load_for_start(&trust, &rig.config_path()).unwrap();
    assert_eq!(loaded.drain_secs, Some(0));
}

/// A program that crashed in the middle of a drain (policy off, no marker) puts the configured
/// policy back when it starts; with the policy as it should be, or a release on trial, it
/// touches nothing.
#[test]
fn a_restart_policy_left_off_by_a_cut_short_drain_is_put_back_at_start() {
    let rig = Rig::new("recover");
    let backend = rig.backend();
    assert_eq!(backend.recover_restart_policy(), Ok(None));
    assert!(rig.steps().is_empty());

    rig.world.0.borrow_mut().container.as_mut().unwrap().restart = "no".to_owned();
    let said = backend.recover_restart_policy().unwrap().unwrap();
    assert!(said.contains("unless-stopped"), "{said}");
    assert_eq!(rig.steps(), ["set_restart:unless-stopped".to_owned()]);
    assert_eq!(rig.world.made().unwrap().restart, "unless-stopped");

    // A release on trial: the revert path owns the container.
    assert!(matches!(rig.update(), Answer::Switched { .. }));
    rig.world.0.borrow_mut().container.as_mut().unwrap().restart = "no".to_owned();
    let before = rig.steps().len();
    assert_eq!(backend.recover_restart_policy(), Ok(None));
    assert_eq!(rig.steps().len(), before);
}

/// At start with no release on trial and no container of that name, there is nothing to put
/// back: the lookup is the only call made to the runtime.
#[test]
fn nothing_is_restored_at_start_when_there_is_no_container() {
    let rig = Rig::new("recover-none");
    rig.world.0.borrow_mut().container = None;
    assert_eq!(rig.backend().recover_restart_policy(), Ok(None));
    assert!(rig.steps().is_empty());
}
