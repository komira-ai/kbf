//! A frontend's `file://` history directory that another process still holds: the start
//! waits for it.
//!
//! The directory's `LOCK` ([`FsStore`]) keeps two servers off one directory. When a
//! frontend is replaced by its next release, the one stopping holds it until it exits,
//! and it exits only once its drain is done, which waits for its open work however long
//! that takes. A service manager may start the replacement before then: Docker Swarm
//! does, one minute after it told the old task to stop, whatever the update's `order`
//! (`stop-first` included) and `stop_grace_period` say. So a start that finds the
//! directory locked by another process ([`FsStoreError::Locked`] with
//! [`TryLockError::WouldBlock`]) does not exit: it logs, and tries again after 250 ms,
//! then after twice as long each time up to 5 s, for as long as the lock is held. There
//! is no deadline, as the drain it waits for has none. Every other failure to open the
//! directory is the result at once, as before.
//!
//! A SIGINT or SIGTERM during the wait ends it: the start fails with [`StoppedWaiting`],
//! which the frontend exits 0 on, as a stop. A Tokio signal listener sees only the
//! signals that come after it is made, so `kbf frontend` makes the watch
//! ([`watch_stops_from_here`]) as soon as it has installed its own handlers, before its
//! start, and a stop sent at any point of the start (the state store's wait included)
//! ends this wait at its first pause. A start that did not make one early (an
//! in-process start) makes its own before the first attempt. The signals are watched
//! only inside a Tokio runtime (every server's start runs in one); outside one the wait
//! only sleeps.
//!
//! Only the frontend waits. A storage node or `all-in-one` whose history directory is
//! held exits 75 ("not ready", `crate::start_exit`), and its unit starts it again.

use std::error::Error;
use std::fmt;
use std::fs::TryLockError;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::task::{Context, Waker};
use std::time::Duration;

use kbf_objstore::{Floor, FsStore, FsStoreError};
use tokio::signal::unix::{Signal, SignalKind, signal};

use super::HistoryFlags;
use super::flags::process_env;
use crate::StateStore;

/// The first pause between two attempts; each pause doubles, up to [`WAIT_MAX`].
const WAIT_FIRST: Duration = Duration::from_millis(250);
const WAIT_MAX: Duration = Duration::from_secs(5);

/// How often a pause looks for a stop signal.
const SLICE: Duration = Duration::from_millis(50);

/// `state` with the history the flags name, as [`super::with_history`] gives it, except
/// that a `file://` directory another process holds is waited for (see the module docs).
///
/// # Errors
/// [`super::HistoryError`] as [`HistoryFlags::open`] gives it, and [`StoppedWaiting`]
/// for a stop signal during the wait.
pub(crate) fn with_history_waiting(
    state: Option<StateStore>,
    flags: &HistoryFlags,
) -> Result<Option<StateStore>, Box<dyn Error>> {
    let mut stopped = None;
    let history = flags.open_by(state.as_ref(), &process_env, &mut |dir| {
        // Before the first attempt: a signal that comes after it, before the first
        // pause, is seen.
        let mut stop = early().unwrap_or_else(Stop::watch);
        let mut attempt = || FsStore::open(dir, Floor::KeepFree);
        let mut pause = |wait| stop.pause(wait);
        open_within(&mut attempt, &mut pause).map_err(|ended| match ended {
            Ended::Failed(e) => e,
            Ended::Stopped(why, last) => {
                stopped = Some(why);
                last
            }
        })
    });
    if let Some(why) = stopped {
        return Err(Box::new(why));
    }
    let history = history?;
    Ok(state.map(|state| StateStore { history, ..state }))
}

/// The watch [`watch_stops_from_here`] made, until a wait takes it. Process-wide, as the
/// signals are: only `kbf frontend`'s `serve` sets it, once, on the thread that then
/// runs the start.
static EARLY: Mutex<Option<Stop>> = Mutex::new(None);

/// Starts watching for SIGINT and SIGTERM now, for the history wait of the start that
/// follows: `kbf frontend` calls it right after installing its handlers.
pub(crate) fn watch_stops_from_here() {
    let stop = Stop::watch();
    *EARLY.lock().unwrap_or_else(PoisonError::into_inner) = Some(stop);
}

/// The watch [`watch_stops_from_here`] made, if any; it is taken.
fn early() -> Option<Stop> {
    EARLY.lock().unwrap_or_else(PoisonError::into_inner).take()
}

/// A stop signal came while the history directory was still locked by another process.
#[derive(Debug)]
pub(crate) struct StoppedWaiting {
    /// The lock file.
    pub path: PathBuf,
    /// How many times the directory was tried.
    pub attempts: u32,
    /// How long the start waited, by the pauses it asked for.
    pub waited: Duration,
}

impl fmt::Display for StoppedWaiting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "--history-objects: stopped by a signal while {} was still locked by another \
             process (tried {} times over {} s)",
            self.path.display(),
            self.attempts,
            self.waited.as_secs()
        )
    }
}

impl Error for StoppedWaiting {}

/// What a pause between two attempts says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Pause {
    /// Try again.
    Again,
    /// A stop signal came: stop waiting.
    Stop,
}

/// Why [`open_within`] gave no store.
#[derive(Debug)]
pub(super) enum Ended {
    /// The directory could not be opened for a reason other than a held lock.
    Failed(FsStoreError),
    /// A pause said stop; with the last attempt's error.
    Stopped(StoppedWaiting, FsStoreError),
}

/// Opens the store `attempt` opens, trying again after each `pause` while another
/// process holds the directory's lock. `pause` is called with each wait. Not generic,
/// as the attempt is not: every test's call counts toward the same function's coverage.
pub(super) fn open_within(
    attempt: &mut dyn FnMut() -> Result<FsStore, FsStoreError>,
    pause: &mut dyn FnMut(Duration) -> Pause,
) -> Result<FsStore, Ended> {
    let mut waited = Duration::ZERO;
    let mut wait = WAIT_FIRST;
    let mut attempts = 0_u32;
    loop {
        attempts += 1;
        let (path, last) = match attempt() {
            Ok(store) => {
                if attempts > 1 {
                    tracing::info!(
                        attempt = attempts,
                        waited_secs = waited.as_secs_f64(),
                        "the history directory is free, opened"
                    );
                }
                return Ok(store);
            }
            Err(e) => match &e {
                FsStoreError::Locked {
                    path,
                    source: TryLockError::WouldBlock,
                } => (path.clone(), e),
                _ => return Err(Ended::Failed(e)),
            },
        };
        if attempts == 1 {
            tracing::info!(
                path = %path.display(),
                "the history directory is locked by another process (a server still \
                 stopping?), waiting for it with no deadline; SIGINT or SIGTERM stops the wait"
            );
        }
        tracing::warn!(
            attempt = attempts,
            waited_secs = waited.as_secs_f64(),
            retry_in_secs = wait.as_secs_f64(),
            path = %path.display(),
            reason = "locked",
            "the history directory is still locked, waiting to try again"
        );
        if pause(wait) == Pause::Stop {
            tracing::info!(
                attempt = attempts,
                waited_secs = waited.as_secs_f64(),
                "a stop signal came while waiting for the history directory, stopping"
            );
            let why = StoppedWaiting {
                path,
                attempts,
                waited,
            };
            return Err(Ended::Stopped(why, last));
        }
        waited += wait;
        wait = (wait * 2).min(WAIT_MAX);
    }
}

/// The SIGINT and SIGTERM a pause looks for, from [`Stop::watch`] on.
pub(super) struct Stop {
    /// `None` outside a Tokio runtime, or when a handler could not be installed.
    signals: Option<(Signal, Signal)>,
}

impl Stop {
    /// Starts watching for SIGINT and SIGTERM, if this thread is inside a Tokio runtime.
    pub(super) fn watch() -> Self {
        let signals = tokio::runtime::Handle::try_current().ok().and_then(|_| {
            let int = signal(SignalKind::interrupt()).ok();
            int.zip(signal(SignalKind::terminate()).ok())
        });
        Self { signals }
    }

    /// Sleeps for `wait`, in slices, and says [`Pause::Stop`] as soon as a signal has
    /// come since [`Stop::watch`].
    pub(super) fn pause(&mut self, wait: Duration) -> Pause {
        let mut left = wait;
        loop {
            if self.stopped() {
                return Pause::Stop;
            }
            if left.is_zero() {
                return Pause::Again;
            }
            let slice = left.min(SLICE);
            std::thread::sleep(slice);
            left -= slice;
        }
    }

    /// Whether a signal has come. Polled with a waker that does nothing: the runtime's
    /// other threads take the signal in, and the next poll sees it.
    fn stopped(&mut self) -> bool {
        let mut cx = Context::from_waker(Waker::noop());
        self.signals.as_mut().is_some_and(|(int, term)| {
            int.poll_recv(&mut cx).is_ready() || term.poll_recv(&mut cx).is_ready()
        })
    }

    /// Whether the signals are watched.
    #[cfg(test)]
    pub(super) fn watching(&self) -> bool {
        self.signals.is_some()
    }
}

#[cfg(test)]
#[path = "lock_wait_tests.rs"]
mod tests;
