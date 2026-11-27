//! `--pack-store-start-wait-secs`: how long each call a frontend makes of the pack store before
//! it serves is made again while the store answers that it is busy or restarting.
//!
//! Before the listeners serve, [`super::FrontendHandle::serve`] checks that the pack store
//! refuses to replace an object ([`check`]) and, under `--executor js-agent`, writes the kbf base
//! tree to it (`base_tree`): a length check and a create-only write for each object of the
//! bundle. Those are the frontend's only calls of the pack store before it serves; nothing else
//! of the start (the flags, the state store, the pack index, the listeners' bind) asks the
//! pack store anything.
//!
//! Each of them is retried as [`kbf_front::start_retry`] says: on a 503 (`SlowDown` among
//! them), a 500, 502 or 504, a 400 `RequestTimeout`, a refused or reset connection or a request
//! that timed out, after jittered waits from 0.5 s doubling to 8 s, each logged with its reason,
//! until the call succeeds (logged with how long it took) or the flag's seconds have passed
//! since its first attempt. Any other answer (a 403, a body that failed its digest check, an
//! object of another length) ends the start at once, exit 2, as before the flag.
//!
//! A call still failing at the bound ends the start with [`ServeError::StartWait`], whose line
//! says the store's last answer, the attempts and how long they took, and exits 1, not 2: the
//! start was not refused, so a unit that restarts on failure and prevents a restart only on
//! exit 2 starts it again (as `--state-store-wait-secs` does for a state store that is not up).
//! 0 retries nothing, and exits 2 as before the flag.
//!
//! Nothing is served while the calls are made: the listeners are bound and answer nothing, so
//! `/readyz` is not ready (a health check that asks it gets no answer) until the calls are done.
//!
//! The default, 120 s, is twice the 60 s within which the frontend's other calls ride out a store
//! that answers 503 ([`kbf_front::store_wait::STORE_WAIT`]). With the default 300 s of
//! `--state-store-wait-secs` before it, a start that waits out both takes up to 420 s, which the
//! start period and retries of a health check of `/readyz` have to cover for the check to not
//! take a waiting frontend for a failed one.

use std::fmt::Display;
use std::time::Duration;

use clap::{Arg, ArgMatches};
use kbf_front::Packs;
use kbf_front::start_retry::{DEFAULT_BOUND, Ended, StartRetry};

use crate::serve::ServeError;

const ID: &str = "pack_store_start_wait_secs";

/// The default in seconds: [`DEFAULT_BOUND`], which clap cannot take as a computed string; a
/// test compares them.
const DEFAULT_SECS: &str = "120";

/// The flag.
pub(super) fn args() -> [Arg; 1] {
    [Arg::new(ID)
        .long("pack-store-start-wait-secs")
        .value_name("SECONDS")
        .value_parser(clap::value_parser!(u64))
        .default_value(DEFAULT_SECS)
        .help(
            "How long, in seconds, each call the frontend makes of the pack store before it \
             serves (the check that the store refuses to replace an object, and, with \
             `--executor js-agent`, each length check and write of the kbf base tree) is made \
             again while the store answers that it is busy or restarting: a 503 (SlowDown \
             among them), a 500, 502 or 504, a 400 RequestTimeout, a refused or reset \
             connection, or a request that timed out. The waits between attempts start at \
             about 0.5 s and double up to 8 s, jittered; each retry is logged with its reason, \
             and a call that succeeds after retries with how long it took. The seconds count \
             from a call's first attempt, the time inside the attempts included. Any other \
             answer (a 403, a body that failed its digest check, an object of another length) \
             is not retried: the frontend exits 2 at once. A call still failing when the \
             seconds have passed makes the frontend exit 1, not 2, with a line that says the \
             store's last answer and how long it was tried. While the calls are made nothing \
             is served and /readyz does not answer. 0 retries nothing, and exits 2 as before \
             this flag. Used only with `--pack-store-endpoint`",
        )]
}

/// The bound the flag in `m` gives (clap gives the default when it is left out).
pub(super) fn from_matches(m: &ArgMatches) -> Duration {
    let secs = m.get_one::<u64>(ID).copied();
    secs.map_or(DEFAULT_BOUND, Duration::from_secs)
}

/// The pack store's check that it refuses to replace an object, if the frontend has packs, made
/// as `retry` says.
///
/// # Errors
/// [`ServeError::Packs`] when the check failed with an answer that is not retried;
/// [`ServeError::StartWait`] when the store kept failing it until the bound.
pub(super) async fn check(packs: Option<&Packs>, retry: &StartRetry) -> Result<(), ServeError> {
    let Some(packs) = packs else {
        return Ok(());
    };
    let checked = packs.check_create_only(retry).await;
    checked.map_err(|ended| match ended {
        Ended::Refused(error) => ServeError::Packs(error.to_string()),
        Ended::GaveUp(gave_up) => gave_up_at("--pack-store-endpoint", retry, &gave_up),
    })
}

/// The start's error for a call of `flag`'s that the store kept failing, as `why` says, until
/// the bound of `retry` passed.
pub(super) fn gave_up_at(flag: &'static str, retry: &StartRetry, why: &dyn Display) -> ServeError {
    ServeError::StartWait {
        flag,
        why: why.to_string(),
        secs: retry.bound().as_secs(),
    }
}

#[cfg(test)]
#[path = "start_wait_tests.rs"]
pub(super) mod tests;
