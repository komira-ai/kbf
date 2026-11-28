//! The job service's completion callback: how a run learns at once that its job ended, with the
//! read-back poll of [`super::run`] kept as the fallback.
//!
//! With `--executor-callback-origin` ([`Config::callback_origin`](super::Config)), every job
//! the executor submits carries a `callback` (jobscheduler's
//! docs/design/job-retry-and-completion-callback.md, section 5.6): the URL
//! `<origin>/js-done/<reference>` and a signing key, both minted for the job just before its
//! first submit ([`Callbacks::arm`]). The reference is 16 random bytes in hex, the key 32 random
//! bytes. They are held in this process's memory only, until the run stops watching the job;
//! no row, flag, metric or log line holds either, and a resend of the submit sends the same
//! spec, so the service makes one job. Without the flag, a job carries no callback, as before.
//!
//! Once the job has reached its final state, the service POSTs to the URL with an empty body,
//! signed by the Standard Webhooks rule: `webhook-signature` is `v1,` and the base64 of
//! HMAC-SHA256 under the key of `<webhook-id>.<webhook-timestamp>.<body>`. [`routes`] serves
//! that path on the operator API, which kbf-proxy forwards to; the call is answered:
//!
//! - **204** when it verifies: the reference is held, the signature is the key's, and the
//!   timestamp is within [`TOLERANCE_SECONDS`] of this host's clock. The run is woken and
//!   reads its job back at once, instead of at its next poll. A `webhook-id` already handled
//!   for the reference is answered 204 again and wakes nothing.
//! - **401** when the headers are missing or malformed, the timestamp is out of the tolerance,
//!   or no signature verifies; nothing changes.
//! - **404** for a reference this process does not hold: another frontend's, or a run that is
//!   over. The service tries a refused call again, with a doubling wait, until the callback's
//!   deadline ([`DEADLINE_SECONDS`] after the job's end), so a call that reached another
//!   frontend of the pool can still reach this one.
//! - **405** for a method other than POST, and **413** for a body over [`MAX_BODY`] bytes (the
//!   service sends none).
//!
//! Nothing here decides an outcome. A wake is a read, and the read decides, as a poll does. A
//! call that never comes ends the run by polling, as before callbacks: a lost call, one
//! refused at every try, a frontend that restarted and so no longer holds the reference, a job
//! taken up from another process (which polls only), and a job service too old to know the
//! field, which skips it. The executor asks for no retry policy (`JobSpec.retry` stays unset):
//! kbf reruns a job itself, under a new id ([`super::outcome`]).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use js_proto::v1::{Callback, JobSpec};
use ring::rand::{SecureRandom, SystemRandom};
use tokio::sync::Notify;
use zeroize::Zeroizing;

/// The path of the completion calls, under the operator API.
const PATH: &str = "/js-done/{reference}";

/// How long after a job's end the service keeps trying its call, in seconds. Polling ends the
/// run well within it; a call past it would wake nothing.
const DEADLINE_SECONDS: u32 = 600;

/// How far a call's `webhook-timestamp` may be from this host's clock, either way, in seconds:
/// the Standard Webhooks tolerance, so a call recorded once cannot be played again later.
const TOLERANCE_SECONDS: u64 = 300;

/// The bytes of a minted signing key.
const KEY_BYTES: usize = 32;

/// The bytes of a minted reference (written in hex).
const REFERENCE_BYTES: usize = 16;

/// The largest body a call is read with. The service sends none.
const MAX_BODY: usize = 4096;

/// `--executor-callback-origin`: the `https` origin the job service reaches this frontend's
/// operator API at, through kbf-proxy: `https://HOST` or `https://HOST:PORT`, nothing after.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallbackOrigin(String);

/// Why a value is not a [`CallbackOrigin`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CallbackOriginError {
    /// The scheme is not `https://`: the job service calls no other.
    #[error("the callback origin must start with https://")]
    NotHttps,
    /// Something other than a host and a port follows the scheme.
    #[error(
        "the callback origin is https://HOST or https://HOST:PORT (a port of 1 to 65535), with no user, path, query or fragment"
    )]
    NotOrigin,
}

impl FromStr for CallbackOrigin {
    type Err = CallbackOriginError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let https = value
            .get(..8)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"));
        let rest = value.get(8..).filter(|_| https);
        let rest = rest.ok_or(CallbackOriginError::NotHttps)?;
        let (host, port) = match rest.rsplit_once(':') {
            // An IPv6 literal with no port: its last colon is inside the brackets.
            Some((host, _)) if !host.ends_with(']') && host.contains(':') => (rest, None),
            Some((host, port)) => (host, Some(port)),
            None => (rest, None),
        };
        let host_ok = !host.is_empty()
            && host.len() <= 253
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-.[]:".contains(&b));
        let port_ok = port.is_none_or(|port| {
            port.len() <= 5
                && port.bytes().all(|b| b.is_ascii_digit())
                && port.parse::<u32>().is_ok_and(|n| (1..=65_535).contains(&n))
        });
        if !(host_ok && port_ok) {
            return Err(CallbackOriginError::NotOrigin);
        }
        Ok(Self(format!("https://{rest}")))
    }
}

impl fmt::Display for CallbackOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The completion calls this process waits for, by reference. Cloned, it is the same set.
#[derive(Clone)]
pub(crate) struct Callbacks {
    shared: Arc<Shared>,
}

struct Shared {
    waiting: Mutex<BTreeMap<String, Waiter>>,
    /// Fills its argument with random bytes; `false` when the source failed.
    fill: fn(&mut [u8]) -> bool,
    /// This host's clock, in seconds since the Unix epoch.
    now: fn() -> u64,
}

/// A run waiting for its job's call.
struct Waiter {
    key: Zeroizing<Vec<u8>>,
    wake: Arc<Notify>,
    /// The `webhook-id`s of the calls that verified.
    handled: BTreeSet<String>,
}

/// Holds a run's place in [`Callbacks`] while it watches its job; dropped, the reference and
/// its key are gone.
pub(super) struct Armed {
    callbacks: Callbacks,
    reference: String,
    wake: Arc<Notify>,
}

impl Drop for Armed {
    fn drop(&mut self) {
        self.callbacks.waiting().remove(&self.reference);
    }
}

/// What a call proves, before its signature is checked.
struct Call<'a> {
    id: &'a str,
    timestamp: &'a str,
    signatures: &'a str,
}

impl Callbacks {
    /// An empty set over the operating system's random source and this host's clock.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::with_sources(system_fill, system_now)
    }

    /// An empty set over the random source `fill` and the clock `now` (seconds since the
    /// epoch): a test's own.
    pub(crate) fn with_sources(fill: fn(&mut [u8]) -> bool, now: fn() -> u64) -> Self {
        Self {
            shared: Arc::new(Shared {
                waiting: Mutex::new(BTreeMap::new()),
                fill,
                now,
            }),
        }
    }

    fn waiting(&self) -> MutexGuard<'_, BTreeMap<String, Waiter>> {
        self.shared
            .waiting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Sets `spec`'s callback to a reference and key minted for it, under `origin`, and holds
    /// them until the returned [`Armed`] is dropped. `None`, and `spec` unchanged, with no
    /// origin, and when the random source fails (logged): the run then watches its job by
    /// polling alone.
    pub(super) fn arm(&self, origin: Option<&CallbackOrigin>, spec: &mut JobSpec) -> Option<Armed> {
        let origin = origin?;
        let mut key = Zeroizing::new(vec![0_u8; KEY_BYTES]);
        let mut reference = [0_u8; REFERENCE_BYTES];
        if !((self.shared.fill)(key.as_mut_slice()) && (self.shared.fill)(&mut reference)) {
            tracing::warn!(
                job = %spec.job_id,
                "the random source failed: the job is watched by polling alone"
            );
            return None;
        }
        let reference = hex::encode(reference);
        spec.callback = Some(Callback {
            url: format!("{origin}/js-done/{reference}"),
            signing_key: key.to_vec(),
            deadline_seconds: Some(DEADLINE_SECONDS),
        });
        Some(self.hold(reference, key))
    }

    /// Holds `reference` under `key` until the returned [`Armed`] is dropped.
    fn hold(&self, reference: String, key: Zeroizing<Vec<u8>>) -> Armed {
        let wake = Arc::new(Notify::new());
        let waiter = Waiter {
            key,
            wake: Arc::clone(&wake),
            handled: BTreeSet::new(),
        };
        self.waiting().insert(reference.clone(), waiter);
        Armed {
            callbacks: self.clone(),
            reference,
            wake,
        }
    }

    /// How many runs wait for a call.
    #[must_use]
    pub(crate) fn waiting_count(&self) -> usize {
        self.waiting().len()
    }

    /// The answer to a call for `reference` with `headers` and `body`, waking its run when it
    /// verifies (see the module documentation).
    #[must_use]
    pub(crate) fn called(&self, reference: &str, headers: &HeaderMap, body: &[u8]) -> StatusCode {
        let mut waiting = self.waiting();
        let Some(waiter) = waiting.get_mut(reference) else {
            return StatusCode::NOT_FOUND;
        };
        let Some(call) = Call::of(headers) else {
            tracing::debug!("a completion call was refused: its headers are missing or malformed");
            return StatusCode::UNAUTHORIZED;
        };
        let now = (self.shared.now)();
        let fresh = call
            .timestamp
            .parse::<u64>()
            .is_ok_and(|at| at.abs_diff(now) <= TOLERANCE_SECONDS);
        if !fresh {
            tracing::debug!(
                "a completion call was refused: its timestamp is not within the tolerance"
            );
            return StatusCode::UNAUTHORIZED;
        }
        if !call.verifies(&waiter.key, body) {
            tracing::debug!("a completion call was refused: no signature verifies");
            return StatusCode::UNAUTHORIZED;
        }
        if waiter.handled.insert(call.id.to_owned()) {
            waiter.wake.notify_one();
        }
        StatusCode::NO_CONTENT
    }
}

impl fmt::Debug for Callbacks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Callbacks")
            .field("waiting", &self.waiting_count())
            .finish_non_exhaustive()
    }
}

impl<'a> Call<'a> {
    /// The three Standard Webhooks headers of a call, each present once, as text, and not empty.
    fn of(headers: &'a HeaderMap) -> Option<Self> {
        let one = |name: &str| {
            let mut values = headers.get_all(name).iter();
            let value = values.next()?.to_str().ok()?;
            (values.next().is_none() && !value.is_empty()).then_some(value)
        };
        Some(Self {
            id: one("webhook-id")?,
            timestamp: one("webhook-timestamp")?,
            signatures: one("webhook-signature")?,
        })
    }

    /// Whether one of the call's `v1,` signatures (separated by spaces) is HMAC-SHA256 under
    /// `key` of `<id>.<timestamp>.<body>`, compared in constant time. A signature of another
    /// version, or not base64, is passed over.
    fn verifies(&self, key: &[u8], body: &[u8]) -> bool {
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key);
        let mut signed = Vec::with_capacity(self.id.len() + self.timestamp.len() + 2 + body.len());
        signed.extend_from_slice(self.id.as_bytes());
        signed.push(b'.');
        signed.extend_from_slice(self.timestamp.as_bytes());
        signed.push(b'.');
        signed.extend_from_slice(body);
        self.signatures
            .split(' ')
            .filter_map(|signature| signature.strip_prefix("v1,"))
            .filter_map(|encoded| STANDARD.decode(encoded).ok())
            .any(|tag| ring::hmac::verify(&key, &signed, &tag).is_ok())
    }
}

/// Completes after `pause` (at once when there is none), or as soon as `armed`'s call has
/// verified, whichever comes first: the wait of a run's reads between two polls.
pub(super) async fn waited(pause: Option<Duration>, armed: Option<&Armed>) {
    let paused = async {
        if let Some(pause) = pause {
            tokio::time::sleep(pause).await;
        }
    };
    tokio::select! {
        biased;
        () = woken(armed) => {}
        () = paused => {}
    }
}

/// Completes once `armed`'s call has verified; never without one. A wake that came while the
/// run was not waiting is kept, so the next wait completes at once.
pub(super) async fn woken(armed: Option<&Armed>) {
    match armed {
        Some(armed) => armed.wake.notified().await,
        None => std::future::pending().await,
    }
}

/// The route of the completion calls, `POST` [`PATH`], answered from `callbacks`. Merged into
/// the operator API of a frontend with an executor.
pub(crate) fn routes(callbacks: Callbacks) -> Router {
    Router::new()
        .route(PATH, post(called))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(callbacks)
}

async fn called(
    State(callbacks): State<Callbacks>,
    Path(reference): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    callbacks.called(&reference, &headers, &body)
}

/// Fills `bytes` from the operating system's random source.
fn system_fill(bytes: &mut [u8]) -> bool {
    SystemRandom::new().fill(bytes).is_ok()
}

/// This host's clock, in seconds since the Unix epoch; 0 for a clock before it.
fn system_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

#[cfg(test)]
#[path = "callback_tests.rs"]
mod tests;
