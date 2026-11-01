//! The runner when the staged release cannot be read for a moment, when it is not what was
//! verified, and when the dead-man could not stop a step.

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use kbf_step_runner::record::State;
use kbf_step_runner::{
    ArtifactError, DeadMan, Finished, LaunchError, Loaded, RunError, StepCall, StepExit,
    StepLauncher, StepRunner,
};
use support::Rig;
use update_hook::{Step, Verdict};

fn halted() -> Finished {
    Finished {
        verdict: Verdict::Halt,
        reason: Some("artifact_changed".to_owned()),
    }
}

fn succeeded() -> Verdict {
    Verdict::Succeeded {
        promotion_pending: false,
    }
}

/// The step and the state the record holds.
fn at(rig: &Rig) -> (Step, State) {
    match rig.store().load().unwrap() {
        Loaded::Current(rec) => (rec.step, rec.state),
        other => panic!("no record: {other:?}"),
    }
}

/// Catches a release that cannot be read for a moment (out of descriptors) halting the service
/// for good as `artifact_changed`: the runner stops with `Unreadable`, writes no verdict, leaves
/// the step pending with nothing run, and the next run checks again and goes on.
#[test]
fn a_release_that_cannot_be_read_is_tried_again_and_never_halts() {
    let rig = Rig::new("runf-io");
    let order = rig.order();
    let failed = Arc::new(AtomicBool::new(false));
    let once = failed.clone();
    let runner =
        rig.runner()
            .with_check_fault(Arc::new(move |step: Step| -> Option<std::io::Error> {
                let fail = step == Step::Cutover && !once.swap(true, Ordering::SeqCst);
                fail.then(|| rustix::io::Errno::MFILE.into())
            }));
    let err = runner.run(&order).unwrap_err();
    assert!(
        matches!(&err, RunError::Unreadable(ArtifactError::Io(_))),
        "{err:?}"
    );
    assert!(err.to_string().contains("still pending"), "{err}");
    assert!(failed.load(Ordering::SeqCst));
    assert_eq!(runner.last_result().unwrap(), None);
    assert_eq!(at(&rig), (Step::Cutover, State::Pending));
    assert_eq!(rig.fake.steps(), [Step::Prepare]);
    assert!(rig.fake.stops().is_empty());

    let got = runner.run(&order).unwrap();
    assert_eq!(got.verdict, succeeded());
    assert_eq!(
        rig.fake.steps(),
        [Step::Prepare, Step::Cutover, Step::Verify, Step::Cleanup]
    );
}

/// Catches a retry that is too wide: a tree that changed, that sits in a place others can write,
/// or that holds what is neither a file, a directory nor a link still halts with
/// `artifact_changed` and runs nothing.
#[test]
fn a_changed_untrusted_or_unsupported_release_still_halts() {
    let hook = Path::new("hook");

    let rig = Rig::new("runf-changed");
    let order = rig.order();
    fs::write(rig.new_dir.join("lib/data"), "tampered").unwrap();
    let err = order.staged.check(hook).unwrap_err();
    assert!(matches!(err, ArtifactError::Changed { .. }), "{err:?}");
    assert_eq!(rig.runner().run(&order).unwrap(), halted());
    assert!(rig.fake.events().is_empty());

    let rig = Rig::new("runf-untrusted");
    let order = rig.order();
    fs::set_permissions(&rig.new_dir, fs::Permissions::from_mode(0o775)).unwrap();
    let err = order.staged.check(hook).unwrap_err();
    assert!(matches!(err, ArtifactError::Untrusted(_)), "{err:?}");
    assert_eq!(rig.runner().run(&order).unwrap(), halted());
    assert!(rig.fake.events().is_empty());

    let rig = Rig::new("runf-unsupported");
    let order = rig.order();
    let sock = rig.new_dir.join("lib/sock");
    drop(UnixListener::bind(&sock).unwrap());
    fs::set_permissions(&sock, fs::Permissions::from_mode(0o755)).unwrap();
    let err = order.staged.check(hook).unwrap_err();
    assert!(matches!(err, ArtifactError::Unsupported(_)), "{err:?}");
    assert_eq!(rig.runner().run(&order).unwrap(), halted());
    assert!(rig.fake.events().is_empty());
}

/// Catches a stop that failed being dropped: when the dead-man cannot stop a step at its bound
/// and the step then ends, the runner stops with `Unstopped` and does not read the step's exit,
/// the record still holds the step as running, and the next run stops the unit by name and runs
/// the step again.
#[test]
fn a_step_the_dead_man_could_not_stop_is_left_running() {
    let rig = Rig::new("runf-unstopped");
    let runner = rig.runner();
    let man = Arc::new(DeadMan::new(
        runner.watch(),
        rig.fake.clone(),
        rig.clock.clone(),
    ));
    let first = Arc::new(AtomicBool::new(true));
    let (clock, seen, once) = (rig.clock.clone(), man.clone(), first.clone());
    rig.fake.on_run(move |call| {
        if call.invocation.step == Step::Cutover && once.swap(false, Ordering::SeqCst) {
            clock.advance(call.timeout_secs);
            assert!(seen.tick().is_err());
        }
    });
    rig.fake.fail_stops(1);
    let err = runner.run(&rig.order()).unwrap_err();
    assert!(
        matches!(&err, RunError::Unstopped { unit, .. } if unit == "updater-svc-cutover-1"),
        "{err:?}"
    );
    assert!(err.to_string().contains("could not stop"), "{err}");
    assert!(!first.load(Ordering::SeqCst));
    assert!(matches!(at(&rig), (Step::Cutover, State::Running { .. })));
    assert_eq!(rig.fake.stops(), ["updater-svc-cutover-1"]);

    let got = runner.run(&rig.order()).unwrap();
    assert_eq!(got.verdict, succeeded());
    assert_eq!(
        rig.fake.stops(),
        ["updater-svc-cutover-1", "updater-svc-cutover-1"]
    );
    assert_eq!(
        rig.fake.steps(),
        [
            Step::Prepare,
            Step::Cutover,
            Step::Cutover,
            Step::Verify,
            Step::Cleanup
        ]
    );
}

/// The rig's launcher, but the first stop says when it has begun and ends only when the test
/// hands it its outcome.
struct HeldFirstStop {
    fake: Arc<support::Fake>,
    held: AtomicBool,
    begun: mpsc::Sender<()>,
    outcome: Mutex<mpsc::Receiver<Result<(), LaunchError>>>,
}

impl StepLauncher for HeldFirstStop {
    fn run(&self, call: &StepCall<'_>) -> Result<StepExit, LaunchError> {
        self.fake.run(call)
    }

    fn stop(&self, unit: &str) -> Result<(), LaunchError> {
        if self.held.swap(false, Ordering::SeqCst) {
            self.begun.send(()).unwrap();
            return self.outcome.lock().unwrap().recv().unwrap();
        }
        self.fake.stop(unit)
    }
}

/// Catches the outcome of a stop that is under way when the step ends being lost: the step's
/// run returns while the dead-man is still in its stop, which then fails. The runner waits for
/// that stop, stops with `Unstopped`, leaves the step running in the record and runs no step
/// after it; the next run stops the unit by name and runs the step again.
#[test]
fn a_stop_that_fails_after_the_step_ended_still_stops_the_runner() {
    let rig = Rig::new("runf-stop-under-way");
    let (begun, has_begun) = mpsc::channel();
    let (finish, outcome) = mpsc::channel();
    let launcher = Arc::new(HeldFirstStop {
        fake: rig.fake.clone(),
        held: AtomicBool::new(true),
        begun,
        outcome: Mutex::new(outcome),
    });
    let runner = StepRunner::new(launcher.clone(), rig.clock.clone(), rig.store());
    let man = Arc::new(DeadMan::new(runner.watch(), launcher, rig.clock.clone()));
    let ticking = Arc::new(Mutex::new(None));
    let has_begun = Mutex::new(has_begun);
    let finish = Mutex::new(Some(finish));
    let first = Arc::new(AtomicBool::new(true));
    let (clock, seen, tick_slot, once) = (
        rig.clock.clone(),
        man.clone(),
        ticking.clone(),
        first.clone(),
    );
    rig.fake.on_run(move |call| {
        if call.invocation.step != Step::Cutover || !once.swap(false, Ordering::SeqCst) {
            return;
        }
        clock.advance(call.timeout_secs);
        let man = seen.clone();
        *tick_slot.lock().unwrap() = Some(std::thread::spawn(move || man.tick()));
        // The stop has begun; it fails once the step has returned its exit.
        has_begun.lock().unwrap().recv().unwrap();
        let finish = finish.lock().unwrap().take().unwrap();
        std::thread::spawn(move || {
            let refused = LaunchError::Launcher("systemd refused".to_owned());
            finish.send(Err(refused)).unwrap();
        });
    });
    let err = runner.run(&rig.order()).unwrap_err();
    assert!(
        matches!(&err, RunError::Unstopped { unit, error }
            if unit == "updater-svc-cutover-1"
                && matches!(&**error, LaunchError::Launcher(why) if why == "systemd refused")),
        "{err:?}"
    );
    let ticked = ticking.lock().unwrap().take().unwrap().join().unwrap();
    assert!(ticked.is_err());
    assert!(!first.load(Ordering::SeqCst));
    assert!(matches!(at(&rig), (Step::Cutover, State::Running { .. })));
    assert_eq!(rig.fake.steps(), [Step::Prepare, Step::Cutover]);
    assert!(rig.fake.stops().is_empty());

    let got = runner.run(&rig.order()).unwrap();
    assert_eq!(got.verdict, succeeded());
    assert_eq!(rig.fake.stops(), ["updater-svc-cutover-1"]);
    assert_eq!(
        rig.fake.steps(),
        [
            Step::Prepare,
            Step::Cutover,
            Step::Cutover,
            Step::Verify,
            Step::Cleanup
        ]
    );
}
