//! The dead-man: a step past its bound is stopped by the root part itself.

mod support;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use kbf_step_runner::{Clock, DeadMan, LaunchError, StepCall, StepExit, StepLauncher, Watch};
use support::{Fake, FakeClock, Reply, Rig};
use update_hook::{Class, Phase, Step, Verdict};

fn deadman(fake: &Arc<Fake>, clock: &Arc<FakeClock>) -> (Arc<Watch>, DeadMan) {
    let watch = Watch::new();
    (
        watch.clone(),
        DeadMan::new(watch, fake.clone(), clock.clone()),
    )
}

/// Catches a dead-man that stops early, never, or twice: a step is left alone until its second, is
/// stopped once at it, and a step that ended (disarmed) or was replaced is not stopped.
#[test]
fn the_armed_step_is_stopped_at_its_second_and_once() {
    let fake = Fake::new();
    let clock = FakeClock::new();
    let (watch, man) = deadman(&fake, &clock);
    assert!(!man.tick().unwrap());
    let start = clock.mono_secs();
    watch.arm("unit-a", start + 10);
    clock.advance(9);
    assert!(!man.tick().unwrap());
    clock.advance(1);
    assert!(man.tick().unwrap());
    assert!(!man.tick().unwrap());
    assert_eq!(fake.stops(), ["unit-a"]);
    // The next step is armed afresh.
    watch.arm("unit-b", start + 20);
    clock.advance(10);
    assert!(man.tick().unwrap());
    assert_eq!(fake.stops(), ["unit-a", "unit-b"]);
    // A step that ended in time is not stopped.
    watch.arm("unit-c", start + 25);
    watch.disarm();
    clock.advance(100);
    assert!(!man.tick().unwrap());
    assert_eq!(fake.stops().len(), 2);
}

/// Catches a stop that fails being counted as done: the step stays armed and the next tick tries
/// again.
#[test]
fn a_stop_that_failed_is_tried_again() {
    let fake = Fake::new();
    let clock = FakeClock::new();
    let (watch, man) = deadman(&fake, &clock);
    watch.arm("unit-a", clock.mono_secs());
    fake.fail_stops(1);
    let err = man.tick().unwrap_err();
    assert!(err.to_string().contains("systemd refused"));
    assert!(man.tick().unwrap());
    assert_eq!(fake.stops(), ["unit-a", "unit-a"]);
}

/// A launcher whose `stop` ends the dead-man's loop, as the test's handshake.
struct StopEnds {
    ends: Arc<AtomicBool>,
    stops: Mutex<Vec<String>>,
    fail: bool,
}

impl StepLauncher for StopEnds {
    fn run(&self, _: &StepCall<'_>) -> Result<StepExit, LaunchError> {
        unreachable!("the dead-man only stops")
    }

    fn stop(&self, unit: &str) -> Result<(), LaunchError> {
        self.stops.lock().unwrap().push(unit.to_owned());
        self.ends.store(true, Ordering::Release);
        if self.fail {
            Err(LaunchError::Launcher("refused".to_owned()))
        } else {
            Ok(())
        }
    }
}

fn thread_run(fail: bool) -> (Arc<Watch>, Arc<StopEnds>) {
    let clock = FakeClock::new();
    let ends = Arc::new(AtomicBool::new(false));
    let launcher = Arc::new(StopEnds {
        ends: ends.clone(),
        stops: Mutex::new(Vec::new()),
        fail,
    });
    let watch = Watch::new();
    watch.arm("unit-t", clock.mono_secs() + 5);
    // The clock moves by the poll at each sleep, so the thread reaches the second in three polls.
    let handle = DeadMan::new(watch.clone(), launcher.clone(), clock).spawn(ends, 2);
    handle.join().unwrap();
    (watch, launcher)
}

/// Catches a dead-man thread that never looks, or that dies on the first error: on a thread of its own it
/// stops the step once its second has passed and then ends when told to; an error of the stop is kept in
/// the watch.
#[test]
fn the_thread_stops_the_step_on_its_own() {
    let (watch, launcher) = thread_run(false);
    assert_eq!(*launcher.stops.lock().unwrap(), ["unit-t"]);
    assert!(watch.last_error().is_none());

    let (watch, launcher) = thread_run(true);
    assert_eq!(*launcher.stops.lock().unwrap(), ["unit-t"]);
    assert!(watch.last_error().unwrap().to_string().contains("refused"));
}

/// Catches a verify window that is not enforced by the root part: with `verify` allowed 1000 s
/// by its own bound but only 300 s by the window, the dead-man stops it at 300; the stopped step
/// ends with no code and the update rolls back.
#[test]
fn a_verify_past_the_window_is_stopped_and_the_update_rolls_back() {
    let rig = Rig::new("dm-window");
    let mut order = rig.order();
    order.bounds.verify_deadline_secs = 300;
    let runner = rig.runner();
    let man = Arc::new(DeadMan::new(
        runner.watch(),
        rig.fake.clone(),
        rig.clock.clone(),
    ));
    rig.fake.say(Step::Verify, &[Reply::Killed]);
    let (clock, seen) = (rig.clock.clone(), man.clone());
    rig.fake.on_run(move |call| {
        if call.invocation.step == Step::Verify {
            clock.advance(299);
            assert!(!seen.tick().unwrap());
            clock.advance(1);
            assert!(seen.tick().unwrap());
        }
    });
    let got = runner.run(&order).unwrap();
    assert_eq!(
        got.verdict,
        Verdict::Failed {
            phase: Phase::GateRelease,
            class: Class::Release
        }
    );
    assert_eq!(rig.fake.stops(), ["updater-svc-verify-1"]);
    assert_eq!(rig.fake.steps().last(), Some(&Step::Rollback));
}

/// Catches a step bound that only the launcher knows: a `cutover` that passes its own bound is
/// stopped by the dead-man, whatever the unprivileged part does.
#[test]
fn a_step_past_its_own_bound_is_stopped() {
    let rig = Rig::new("dm-bound");
    let runner = rig.runner();
    let man = Arc::new(DeadMan::new(
        runner.watch(),
        rig.fake.clone(),
        rig.clock.clone(),
    ));
    rig.fake.say(Step::Cutover, &[Reply::Killed]);
    let (clock, seen) = (rig.clock.clone(), man.clone());
    rig.fake.on_run(move |call| {
        if call.invocation.step == Step::Cutover {
            clock.advance(call.timeout_secs);
            assert!(seen.tick().unwrap());
        }
    });
    let got = runner.run(&rig.order()).unwrap();
    assert_eq!(
        got.verdict,
        Verdict::Failed {
            phase: Phase::BeforeCommit,
            class: Class::Release
        }
    );
    assert_eq!(rig.fake.stops(), ["updater-svc-cutover-1"]);
}

/// A launcher whose `stop` is slow: while it works, the next run of the same step is armed.
struct RearmsDuringStop {
    watch: Arc<Watch>,
    next_deadline: u64,
    stops: Mutex<Vec<String>>,
}

impl StepLauncher for RearmsDuringStop {
    fn run(&self, _: &StepCall<'_>) -> Result<StepExit, LaunchError> {
        unreachable!("the dead-man only stops")
    }

    fn stop(&self, unit: &str) -> Result<(), LaunchError> {
        self.stops.lock().unwrap().push(unit.to_owned());
        // The runner started the step again under the same unit name.
        self.watch.arm(unit, self.next_deadline);
        Ok(())
    }
}

/// Catches a stop that is marked on the step armed after it: the unit name is the same for the
/// next try of a step, so the mark is keyed on the arming. The new arm is still enforced when
/// its own second comes.
#[test]
fn a_stop_does_not_disarm_the_step_armed_while_it_ran() {
    let clock = FakeClock::new();
    let watch = Watch::new();
    let start = clock.mono_secs();
    let launcher = Arc::new(RearmsDuringStop {
        watch: watch.clone(),
        next_deadline: start + 100,
        stops: Mutex::new(Vec::new()),
    });
    let man = DeadMan::new(watch.clone(), launcher.clone(), clock.clone());
    watch.arm("unit-r", start + 5);
    clock.advance(5);
    assert!(man.tick().unwrap());
    assert!(!man.tick().unwrap());
    clock.advance(100);
    assert!(man.tick().unwrap());
    assert_eq!(*launcher.stops.lock().unwrap(), ["unit-r", "unit-r"]);
}

/// Catches a stop that failed being forgotten once the step ends: the runner learns from
/// `disarm` that the dead-man could not stop the step at its bound; a stop that worked at a later
/// try, or a step that was never due, leaves nothing to report.
#[test]
fn a_failed_stop_is_reported_when_the_step_is_disarmed() {
    let fake = Fake::new();
    let clock = FakeClock::new();
    let (watch, man) = deadman(&fake, &clock);
    watch.arm("unit-a", clock.mono_secs());
    fake.fail_stops(1);
    assert!(man.tick().is_err());
    let error = watch.disarm().expect("the failed stop is reported");
    assert!(
        matches!(&*error, LaunchError::Launcher(why) if why == "systemd refused"),
        "{error:?}"
    );

    watch.arm("unit-b", clock.mono_secs());
    fake.fail_stops(1);
    assert!(man.tick().is_err());
    assert!(man.tick().unwrap());
    assert!(watch.disarm().is_none());

    watch.arm("unit-c", clock.mono_secs() + 100);
    assert!(!man.tick().unwrap());
    assert!(watch.disarm().is_none());
    assert_eq!(fake.stops(), ["unit-a", "unit-b", "unit-b"]);
}

/// A launcher whose stop always fails.
struct FailsToStop;

impl StepLauncher for FailsToStop {
    fn run(&self, _: &StepCall<'_>) -> Result<StepExit, LaunchError> {
        unreachable!("the dead-man only stops")
    }

    fn stop(&self, _: &str) -> Result<(), LaunchError> {
        Err(LaunchError::Launcher("refused".to_owned()))
    }
}

/// A fake clock that disarms the watch at its first sleep (the step ended after the failed
/// stop) and asks the dead-man's loop to end at its `n`-th.
struct EndsAt {
    clock: Arc<FakeClock>,
    watch: Arc<Watch>,
    stop: Arc<AtomicBool>,
    sleeps_left: AtomicU32,
}

impl Clock for EndsAt {
    fn mono_secs(&self) -> u64 {
        self.clock.mono_secs()
    }

    fn boot_id(&self) -> String {
        self.clock.boot_id()
    }

    fn sleep_secs(&self, secs: u64) {
        self.clock.sleep_secs(secs);
        let _ = self.watch.disarm();
        if self.sleeps_left.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.stop.store(true, Ordering::Release);
        }
    }
}

/// Catches the error of a stop being wiped by the next tick that has nothing to stop (the error
/// was then never seen): it stays in the watch, as the launcher's error, until a stop works.
#[test]
fn the_error_of_a_stop_outlives_the_ticks_after_it() {
    let watch = Watch::new();
    let stop = Arc::new(AtomicBool::new(false));
    let clock = Arc::new(EndsAt {
        clock: FakeClock::new(),
        watch: watch.clone(),
        stop: stop.clone(),
        sleeps_left: AtomicU32::new(2),
    });
    watch.arm("unit-e", clock.mono_secs());
    DeadMan::new(watch.clone(), Arc::new(FailsToStop), clock).run(&stop, 1);
    let error = watch.last_error().expect("the error is kept");
    assert!(
        matches!(&*error, LaunchError::Launcher(why) if why == "refused"),
        "{error:?}"
    );
}

/// A launcher whose stop says when it has begun and ends, with the error it is handed, only
/// when the test lets it.
struct HeldStop {
    begun: mpsc::Sender<()>,
    outcome: Mutex<mpsc::Receiver<Result<(), LaunchError>>>,
}

impl StepLauncher for HeldStop {
    fn run(&self, _: &StepCall<'_>) -> Result<StepExit, LaunchError> {
        unreachable!("the dead-man only stops")
    }

    fn stop(&self, _: &str) -> Result<(), LaunchError> {
        self.begun.send(()).unwrap();
        self.outcome.lock().unwrap().recv().unwrap()
    }
}

/// A dead-man over a [`HeldStop`].
struct Held {
    watch: Arc<Watch>,
    man: Arc<DeadMan>,
    /// A message when the stop has begun.
    has_begun: mpsc::Receiver<()>,
    /// The outcome the stop ends with.
    finish: mpsc::Sender<Result<(), LaunchError>>,
}

fn held_stop() -> Held {
    let (begun, has_begun) = mpsc::channel();
    let (finish, outcome) = mpsc::channel();
    let watch = Watch::new();
    let launcher = Arc::new(HeldStop {
        begun,
        outcome: Mutex::new(outcome),
    });
    let man = Arc::new(DeadMan::new(watch.clone(), launcher, FakeClock::new()));
    Held {
        watch,
        man,
        has_begun,
        finish,
    }
}

/// Catches the outcome of a stop under way being lost when the step ends meanwhile (the arming
/// was dropped, so the stop's error had nowhere to go and the runner read the exit of a step it
/// could not stop): `disarm` waits for the stop and gives its error; a stop that works gives
/// none. Nothing here waits for a time: the stop has begun, and is seen to have begun, before
/// the disarm is made.
#[test]
fn a_stop_under_way_when_the_step_ends_is_waited_for() {
    for (outcome, reported) in [
        (Err(LaunchError::Launcher("refused".to_owned())), true),
        (Ok(()), false),
    ] {
        let Held {
            watch,
            man,
            has_begun,
            finish,
        } = held_stop();
        watch.arm("unit-h", 0);
        assert!(!watch.stopping());
        let ticking = {
            let man = man.clone();
            std::thread::spawn(move || man.tick())
        };
        has_begun.recv().unwrap();
        assert!(watch.stopping());
        let disarming = {
            let watch = watch.clone();
            std::thread::spawn(move || watch.disarm())
        };
        finish.send(outcome).unwrap();
        let error = disarming.join().unwrap();
        assert_eq!(error.is_some(), reported, "{error:?}");
        assert_eq!(ticking.join().unwrap().is_err(), reported);
        assert!(!watch.stopping());
        assert!(watch.disarm().is_none());
    }
}
