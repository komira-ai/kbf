//! The check of the new release's own configuration at the start of a switch, over a fake
//! runtime: a release that refuses the container's configuration changes nothing.

use super::tests::{CONFIG, NEW, NEW_ID, OLD_ID, Rig, failed, refused};
use crate::local::answer::{Answer, Code};

const REFUSED_TEXT: &str = "--history-objects is not a flag of this release";

/// What a refused switch leaves: the runtime saw only the load of the image (no restart policy
/// change, drain, stop, remove or run), the old container runs with its policy, and nothing is
/// on trial.
fn nothing_changed(rig: &Rig) {
    assert_eq!(rig.steps(), ["fetch".to_owned(), "load".to_owned()]);
    let made = rig.world.made().unwrap();
    assert_eq!((made.image_id.as_str(), made.running), (OLD_ID, true));
    assert_eq!(made.restart, "unless-stopped");
    assert!(!made.draining);
    assert!(rig.record().pending.is_none());
    assert!(rig.world.runs().is_empty());
}

/// A check that exits 2 refuses the switch as `config_invalid` with the release and what the
/// check printed, and nothing was changed.
#[test]
fn a_check_that_refuses_the_configuration_refuses_the_switch() {
    let rig = Rig::new("check-refused");
    rig.world.check_fails(NEW_ID, 2, REFUSED_TEXT);
    let answer = rig.update();
    assert!(refused(Code::ConfigInvalid)(&answer), "{answer:?}");
    let Answer::Refused(_, detail) = &answer else {
        unreachable!()
    };
    assert_eq!(
        detail,
        &format!(
            "release {NEW} refuses this container's configuration (its check ended with exit status 2): {REFUSED_TEXT}"
        )
    );
    nothing_changed(&rig);
}

/// A check that exits 75 (not ready) refuses the switch the same way: the old container keeps
/// serving and the operator asks again.
#[test]
fn a_check_that_is_not_ready_refuses_the_switch() {
    let rig = Rig::new("check-not-ready");
    rig.world
        .check_fails(NEW_ID, 75, "the certificate is not yet valid");
    let answer = rig.update();
    assert!(refused(Code::ConfigInvalid)(&answer), "{answer:?}");
    assert!(
        matches!(&answer, Answer::Refused(_, d) if d.contains("exit status 75") && d.ends_with("the certificate is not yet valid")),
        "{answer:?}"
    );
    nothing_changed(&rig);
}

/// The check runs once, with the new image and not the old, after the image is loaded and
/// before anything else happens; a passing check changes nothing of the switch.
#[test]
fn the_check_runs_once_with_the_new_image_before_anything_else() {
    let rig = Rig::new("check-passes");
    assert!(matches!(rig.update(), Answer::Switched { .. }));
    // Two events (fetch, load) came before it.
    assert_eq!(rig.world.checks(), [(NEW_ID.to_owned(), 2)]);
    assert_eq!(rig.world.made().unwrap().image_id, NEW_ID);
}

/// The check comes before the peers are asked: a refusing check with a peer that is not ready
/// answers `config_invalid`, and the peer was never looked at.
#[test]
fn the_check_is_before_the_peer_probe() {
    let config = CONFIG.replace("api =", "peers = [\"203.0.113.11:8982\"]\napi =");
    let rig = Rig::with_config("check-before-peer", &config);
    rig.world.check_fails(NEW_ID, 2, REFUSED_TEXT);
    let answer = rig.update();
    assert!(refused(Code::ConfigInvalid)(&answer), "{answer:?}");
    assert!(
        !rig.world.events().iter().any(|e| e.starts_with("get:")),
        "{:?}",
        rig.world.events()
    );
    nothing_changed(&rig);
}

/// Once the file is fixed, the same request goes through.
#[test]
fn asking_again_after_the_check_passes_switches() {
    let rig = Rig::new("check-then-pass");
    rig.world.check_fails(NEW_ID, 2, REFUSED_TEXT);
    assert!(refused(Code::ConfigInvalid)(&rig.update()));
    rig.world.0.borrow_mut().check_says.clear();
    assert!(matches!(rig.switch(), Answer::Switched { .. }));
    assert_eq!(rig.world.made().unwrap().image_id, NEW_ID);
}

/// Docker failing to run the check is `runtime_failed` (the runtime, not the release, said
/// no), and nothing was changed either.
#[test]
fn docker_failing_to_run_the_check_fails_with_runtime_failed() {
    let rig = Rig::new("check-docker-fails");
    rig.world.fail("check");
    let answer = rig.update();
    assert!(failed(Code::RuntimeFailed)(&answer), "{answer:?}");
    nothing_changed(&rig);
}
