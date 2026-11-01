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
//! A stop under way is part of the arming ([`Watch::stopping`]): a step that ends while the
//! dead-man is stopping it (it ended on its own just as its second came) is not disarmed until
//! that stop has ended, so its outcome reaches the runner either way. A stop that fails then
//! is [`crate::RunError::Unstopped`] as any other; one that works lets the runner read the exit.
//! The launcher's `stop` therefore never disarms the watch itself: it would wait for its own end.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use crate::clock::Clock;
use crate::launcher::{LaunchError, StepLauncher};

struct Active {
    /// Which arming this is: the unit name repeats for the next try of a step.
    arm: u64,
    unit: String,
    deadline_secs: u64,
    stopped: bool,
    /// The dead-man is in [`StepLauncher::stop`] for this arming.
    stopping: bool,
    /// The error of the last try to stop this arming, `None` once a stop worked.
    stop_error: Option<Arc<LaunchError>>,
}

#[derive(Default)]
struct WatchState {
    armed: u64,
    active: Option<Active>,
    last_error: Option<Arc<LaunchError>>,
}

/// What the runner tells the dead-man.
#[derive(Default)]
pub struct Watch {
    state: Mutex<WatchState>,
    /// Signalled when a stop under way has ended.
    settled: Condvar,
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
            stopping: false,
            stop_error: None,
        });
    }

    /// The step ended. `Some` with the error of the dead-man's stop when the step passed its
    /// bound and the dead-man could not stop it. A stop under way is waited for first, so that
    /// its outcome is not lost with the arming.
    pub fn disarm(&self) -> Option<Arc<LaunchError>> {
        let mut state = self.state();
        while state.active.as_ref().is_some_and(|a| a.stopping) {
            state = self
                .settled
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        state.active.take().and_then(|a| a.stop_error)
    }

    /// Whether the dead-man is stopping the armed step now.
    pub fn stopping(&self) -> bool {
        self.state().active.as_ref().is_some_and(|a| a.stopping)
    }

    /// The last error of stopping a unit, `None` when the last stop worked. A tick that has
    /// nothing to stop leaves it as it is.
    pub fn last_error(&self) -> Option<Arc<LaunchError>> {
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
    pub fn tick(&self) -> Result<bool, Arc<LaunchError>> {
        let due = {
            let mut state = self.watch.state();
            let now = self.clock.mono_secs();
            let due = state
                .active
                .as_mut()
                .filter(|a| !a.stopped && !a.stopping && now >= a.deadline_secs);
            due.map(|a| {
                a.stopping = true;
                (a.unit.clone(), a.arm)
            })
        };
        let Some((unit, arm)) = due else {
            return Ok(false);
        };
        let error = self.launcher.stop(&unit).err().map(Arc::new);
        // Only the arming that was stopped: the next try of the step may be armed meanwhile.
        let mut state = self.watch.state();
        let armed = state.active.iter_mut().filter(|a| a.arm == arm);
        armed.for_each(|a| {
            a.stopping = false;
            a.stopped = error.is_none();
            a.stop_error.clone_from(&error);
        });
        drop(state);
        self.watch.settled.notify_all();
        error.map_or(Ok(true), Err)
    }

    /// Ticks every `poll_secs` until `stop` is set. An error of stopping a unit is kept in the
    /// watch ([`Watch::last_error`]) until a stop works, and the next tick tries again.
    pub fn run(&self, stop: &AtomicBool, poll_secs: u64) {
        while !stop.load(Ordering::Acquire) {
            match self.tick() {
                Ok(false) => {}
                Ok(true) => self.watch.state().last_error = None,
                Err(e) => self.watch.state().last_error = Some(e),
            }
            self.clock.sleep_secs(poll_secs);
        }
    }

    /// Runs [`DeadMan::run`] on a thread of its own.
    pub fn spawn(self, stop: Arc<AtomicBool>, poll_secs: u64) -> JoinHandle<()> {
        std::thread::spawn(move || self.run(&stop, poll_secs))
    }
}
