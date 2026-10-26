//! `--state-store-wait-secs`: how long a frontend waits, at start, for a state store that
//! is not up yet.
//!
//! A host that reboots starts the frontend and its Postgres together, and the frontend is
//! often first. Without a wait it exits 2 ("a start the frontend refuses"), which a unit
//! with `RestartPreventExitStatus=2` never retries. With one, a store that is not up yet
//! ([`OpenFailure::NotUp`]: connection refused or reset, a Unix socket that is not made
//! yet, a connect that timed out, SQLSTATE 57P03) is opened again after 250 ms, then after
//! twice as long each time up to 10 s, until the window is used up. Any refusal
//! ([`OpenFailure::Refused`]: a URL, a login, a database, a schema) is the result at once,
//! exit 2 as ever, and so is any failure with a window of 0.
//!
//! A window used up with the store still not up is [`NotReached`], which exits 1: the
//! start was not refused, so a unit that restarts on failure and prevents a restart only
//! on exit 2 tries it again.
//!
//! The wait blocks the thread, as the wait for a late listener address does
//! ([`crate::listen::Binder`]); a stop signal sent during it is acted on once the start
//! has come to its end.

use std::error::Error;
use std::fmt;
use std::time::Duration;

use clap::{Arg, ArgMatches};

use crate::{OpenFailure, StateStore, StateStoreUri};

const ID: &str = "state_store_wait_secs";

/// The default window in seconds: more than a Postgres container needs to start after a
/// reboot, and less than a person waits before looking.
const DEFAULT_SECS: &str = "300";

/// The most seconds the flag takes.
const MAX_SECS: u64 = 3600;

/// The first wait between two attempts; each wait doubles, up to [`WAIT_MAX`].
const WAIT_FIRST: Duration = Duration::from_millis(250);
const WAIT_MAX: Duration = Duration::from_secs(10);

/// The flag.
pub(super) fn args() -> [Arg; 1] {
    [Arg::new(ID)
        .long("state-store-wait-secs")
        .value_name("SECONDS")
        .value_parser(clap::value_parser!(u64).range(0..=MAX_SECS))
        .default_value(DEFAULT_SECS)
        .help(
            "How long, in seconds, the start waits for a Postgres `--state-store` that is not \
             up yet (connection refused or reset, a Unix socket that does not exist yet, a \
             connect that times out, or the server answering that it is starting up), trying \
             again after 250 ms and then after twice as long each time, up to 10 s. A store \
             that refuses (a URL that does not parse, a login, a missing database, a schema \
             this build does not accept) is not waited for: the frontend exits 2 at once. If \
             the window is used up with the store still not up, the frontend exits 1, not 2, \
             so that a unit which restarts on failure and prevents a restart on exit 2 \
             starts it again. 0 does not wait, and exits 2 as it did before this flag. At \
             most 3600.",
        )]
}

/// The window the flag in `m` gives.
pub(super) fn from_matches(m: &ArgMatches) -> Duration {
    Duration::from_secs(*m.get_one::<u64>(ID).expect("the flag has a default"))
}

/// The window was used up and the store was still not up.
#[derive(Debug)]
pub struct NotReached {
    /// How many times the store was tried.
    pub attempts: u32,
    /// How long the start waited, by the waits it asked for.
    pub waited: Duration,
    /// What the last attempt said.
    pub last: crate::farm::StateError,
}

impl fmt::Display for NotReached {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the state store was still not reachable after {} attempts over {} s \
             (--state-store-wait-secs): {}",
            self.attempts,
            self.waited.as_secs(),
            self.last
        )
    }
}

impl Error for NotReached {}

/// Opens the state store `uri` names, waiting up to `window` for a server that is not up
/// yet.
pub(super) fn open(uri: &StateStoreUri, window: Duration) -> Result<StateStore, Box<dyn Error>> {
    open_within(window, &mut || uri.try_open(), &mut std::thread::sleep)
}

/// [`open`] over any attempt and any sleep: `sleep` is called with each wait, and the
/// waits add up to at most `window`. Not generic, as the attempt is not: every test's call
/// counts toward the same function's coverage.
pub(super) fn open_within(
    window: Duration,
    attempt: &mut dyn FnMut() -> Result<StateStore, OpenFailure>,
    sleep: &mut dyn FnMut(Duration),
) -> Result<StateStore, Box<dyn Error>> {
    let mut waited = Duration::ZERO;
    let mut wait = WAIT_FIRST;
    let mut attempts = 0_u32;
    loop {
        attempts += 1;
        let last = match attempt() {
            Ok(store) => {
                if attempts > 1 {
                    tracing::info!(
                        attempt = attempts,
                        waited_secs = waited.as_secs_f64(),
                        "the state store is reachable, opened"
                    );
                }
                return Ok(store);
            }
            Err(OpenFailure::Refused(e)) => return Err(Box::new(e)),
            Err(OpenFailure::NotUp(e)) if window.is_zero() => return Err(Box::new(e)),
            Err(OpenFailure::NotUp(e)) => e,
        };
        let left = window - waited;
        if left.is_zero() {
            tracing::error!(
                attempt = attempts,
                waited_secs = waited.as_secs_f64(),
                reason = "not_up",
                error = %last,
                "the state store is still not reachable, giving up"
            );
            return Err(Box::new(NotReached {
                attempts,
                waited,
                last,
            }));
        }
        let pause = wait.min(left);
        tracing::warn!(
            attempt = attempts,
            waited_secs = waited.as_secs_f64(),
            retry_in_secs = pause.as_secs_f64(),
            reason = "not_up",
            error = %last,
            "the state store is not reachable yet, waiting to try again"
        );
        sleep(pause);
        waited += pause;
        wait = (wait * 2).min(WAIT_MAX);
    }
}
