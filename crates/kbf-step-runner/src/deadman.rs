//! The dead-man: the bound of the running step, enforced on a thread of its own.
//!
//! The runner arms the watch with the unit it is about to start and the second (on the
//! monotonic clock) the step must end by, and disarms it when the step has ended. The dead-man
//! looks at the watch and, when the second has passed with the step still armed, stops its
//! unit. It does not wait for the runner, which is blocked in the launcher for as long as the
//! step lives, and it does not wait for the unprivileged part of the updater, which may be gone
//! (docs/design/host-updates.md, 3.5): the root part brings the host back by itself. The step
//! that was stopped ends without an exit code, which the runner reads as a failure and runs
//! `rollback` for.
//!
//! For `verify` the runner arms the earlier of the step's bound and the end of the verify
//! window, so a `verify` that is still running at `verify_deadline_secs` is stopped here and the
//! update rolls back.
//!
//! A stop that fails is tried again at every tick, and kept on the arming: when the step ends
//! before a stop has worked, [`Watch::disarm`] gives the runner the error, and the runner stops
//! with [`crate::RunError::Unstopped`] rather than read a step that ran past its bound. The
//! dead-man waits in [`StepLauncher::stop`] for as long as it takes, so the launcher bounds its
//! stop; and if the root part itself is killed, the dead-man dies with it, so the launcher also
//! bounds the unit itself (see [`StepLauncher`]).
//!
//! One window is left: a stop that is under way when the step ends and the runner disarms (the
//! step ended on its own just as its second came) is kept in [`Watch::last_error`] only if it
//! fails, not on the arming, which is gone. The step has ended by then, so no unit is left
//! running; the runner reads its exit as it came.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use crate::clock::Clock;
use crate::launcher::{LaunchError, StepLauncher};

struct Active {
    /// Which arming this is: the unit name repeats for the next try of a step.
    arm: u64,
    unit: String,
    deadline_secs: u64,
    stopped: bool,
    /// The error of the last try to stop this arming, `None` once a stop worked.
    stop_error: Option<String>,
}

#[derive(Default)]
struct WatchState {
    armed: u64,
    active: Option<Active>,
    last_error: Option<String>,
}

/// What the runner tells the dead-man.
#[derive(Default)]
pub struct Watch {
    state: Mutex<WatchState>,
}

impl Watch {
    pub fn new() -> Arc<Watch> {
        Arc::new(Watch::default())
    }

    fn state(&self) -> MutexGuard<'_, WatchState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The step in `unit` must end by `deadline_secs` on the monotonic clock.
    pub fn arm(&self, unit: &str, deadline_secs: u64) {
        let mut state = self.state();
        state.armed += 1;
        let arm = state.armed;
        state.active = Some(Active {
            arm,
            unit: unit.to_owned(),
            deadline_secs,
            stopped: false,
            stop_error: None,
        });
    }

    /// The step ended. `Some` with the error of the dead-man's stop when the step passed its
    /// bound and the dead-man could not stop it.
    pub fn disarm(&self) -> Option<String> {
        self.state().active.take().and_then(|a| a.stop_error)
    }

    /// The last error of stopping a unit, `None` when the last stop worked. A tick that has
    /// nothing to stop leaves it as it is.
    pub fn last_error(&self) -> Option<String> {
        self.state().last_error.clone()
    }
}

/// Enforces the deadline of the armed step.
pub struct DeadMan {
    watch: Arc<Watch>,
    launcher: Arc<dyn StepLauncher>,
    clock: Arc<dyn Clock>,
}

impl DeadMan {
    pub fn new(watch: Arc<Watch>, launcher: Arc<dyn StepLauncher>, clock: Arc<dyn Clock>) -> Self {
        DeadMan {
            watch,
            launcher,
            clock,
        }
    }

    /// Looks at the watch once: stops the armed step if its second has passed. `true` when it
    /// stopped one. A step is stopped once; a stop that failed is tried again at the next tick.
    pub fn tick(&self) -> Result<bool, LaunchError> {
        let due = {
            let state = self.watch.state();
            state
                .active
                .as_ref()
                .filter(|a| !a.stopped && self.clock.mono_secs() >= a.deadline_secs)
                .map(|a| (a.unit.clone(), a.arm))
        };
        let Some((unit, arm)) = due else {
            return Ok(false);
        };
        let stopped = self.launcher.stop(&unit);
        let error = stopped.as_ref().err().map(ToString::to_string);
        // Only the arming that was stopped: the next try of the step may be armed meanwhile.
        let mut state = self.watch.state();
        let armed = state.active.iter_mut().filter(|a| a.arm == arm);
        armed.for_each(|a| {
            a.stopped = error.is_none();
            a.stop_error.clone_from(&error);
        });
        stopped.map(|()| true)
    }

    /// Ticks every `poll_secs` until `stop` is set. An error of stopping a unit is kept in the
    /// watch ([`Watch::last_error`]) until a stop works, and the next tick tries again.
    pub fn run(&self, stop: &AtomicBool, poll_secs: u64) {
        while !stop.load(Ordering::Acquire) {
            match self.tick() {
                Ok(false) => {}
                Ok(true) => self.watch.state().last_error = None,
                Err(e) => self.watch.state().last_error = Some(e.to_string()),
            }
            self.clock.sleep_secs(poll_secs);
        }
    }

    /// Runs [`DeadMan::run`] on a thread of its own.
    pub fn spawn(self, stop: Arc<AtomicBool>, poll_secs: u64) -> JoinHandle<()> {
        std::thread::spawn(move || self.run(&stop, poll_secs))
    }
}
