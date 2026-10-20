//! A run that is asked to stop: SIGTERM (a CI job's timeout or cancel gives the runner a
//! chance to end its steps), SIGINT and SIGHUP set a flag that every wait of the harness
//! looks at, so the run ends through its normal exits (the farm is stopped, the buck2
//! daemons are killed, the result document is finished as failed) instead of leaving
//! processes behind. Without a handler these signals would end the harness at once, with
//! none of that run.
//!
//! [`Interrupt::process`] is the flag the handlers set (what `kbf-beta` uses after
//! [`install`]); [`Interrupt::new`] is a flag of its own, for a test, that only the test
//! sets. A harness killed with SIGKILL gets no chance to run anything: its processes die
//! with it through the kernel (`crate::procs`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set by the signal handler.
static PROCESS: AtomicBool = AtomicBool::new(false);

/// A flag that says the run was asked to stop.
#[derive(Clone, Debug)]
pub struct Interrupt {
    own: Option<Arc<AtomicBool>>,
}

impl Interrupt {
    /// A flag of its own, set by [`Interrupt::set`] only.
    #[must_use]
    pub fn new() -> Self {
        Self {
            own: Some(Arc::new(AtomicBool::new(false))),
        }
    }

    /// The flag the signal handlers set ([`install`]).
    #[must_use]
    pub fn process() -> Self {
        Self { own: None }
    }

    /// Whether the run was asked to stop.
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.own
            .as_ref()
            .map_or(&PROCESS, |own| own.as_ref())
            .load(Ordering::Relaxed)
    }

    /// Asks the run to stop, as a signal does.
    pub fn set(&self) {
        self.own
            .as_ref()
            .map_or(&PROCESS, |own| own.as_ref())
            .store(true, Ordering::Relaxed);
    }
}

impl Default for Interrupt {
    fn default() -> Self {
        Self::new()
    }
}

/// The handler: one atomic store, which is async-signal-safe.
extern "C" fn on_signal(_signal: libc::c_int) {
    PROCESS.store(true, Ordering::Relaxed);
}

/// Makes SIGTERM, SIGINT and SIGHUP set [`Interrupt::process`]. Call once, at the start of
/// the program.
pub fn install() {
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        // SAFETY: `on_signal` is an `extern "C"` function that only stores to an atomic,
        // which is async-signal-safe; `signal` is a valid signal number.
        unsafe {
            libc::signal(signal, on_signal as *const () as libc::sighandler_t);
        }
    }
}

#[cfg(test)]
#[path = "interrupt_tests.rs"]
mod tests;
