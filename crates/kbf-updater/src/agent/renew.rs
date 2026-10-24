//! The renewal of the updater's certificate, by the agent, at half its life
//! (`docs/design/updates.md`, section 5; `docs/design/install-and-enrollment.md`, section 5.8).
//!
//! It is the node's renewal (`kbf_enroll::renew`): a request signed by the key `kbf-updater
//! enroll` made, which the registry holds, with no token and no certificate, so a host whose
//! certificate lapsed during an outage still recovers. The new certificate (the key does not
//! change) replaces the old in the state directory in one step, and the agent's heartbeats and
//! its calls to the frontend read the files every time, so the next one uses it. A failure
//! retries after 5 minutes, doubling to an hour, and is logged; below a day to expiry it is
//! logged as an error.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_enroll::client::connect;
use kbf_enroll::config::FrontendUrl;
use kbf_enroll::pin::Anchor;
use kbf_enroll::renew::{Backoff, first_attempt, near_expiry, renew};
use kbf_enroll::store::{Node, Store};

use super::enroll::text;

/// The longest one wait lasts, in seconds: a wall clock that jumped is noticed within it.
pub const NAP_MAX_S: u64 = 60;

/// Keeps the updater's certificate fresh.
pub trait Renewer: Send + Sync {
    /// Does what is due (a renewal, if its time has come) and returns how long to wait before
    /// looking again.
    fn round(&self) -> Duration;
}

/// When the next renewal is due, and how long to wait after a failure.
struct Schedule {
    /// Unix seconds.
    due: i64,
    backoff: Backoff,
}

/// The renewer that calls the frontend.
pub struct FrontendRenewer {
    url: FrontendUrl,
    store: Store,
    runtime: tokio::runtime::Runtime,
    now: Arc<dyn Fn() -> i64 + Send + Sync>,
    log: Arc<dyn Fn(&str) + Send + Sync>,
    /// The spread of the schedule, in thousandths.
    jitter: i32,
    schedule: Mutex<Schedule>,
}

impl FrontendRenewer {
    /// A renewer for the host enrolled in `store`, whose first renewal is due at the
    /// certificate's `renew_after` spread by `jitter_permille` (clamped to plus or minus 10%).
    ///
    /// # Errors
    /// The host is not enrolled in `store` (there is no `node.json`: run `kbf-updater enroll`), or
    /// a file cannot be read.
    pub fn new(
        url: FrontendUrl,
        store: Store,
        now: Arc<dyn Fn() -> i64 + Send + Sync>,
        log: Arc<dyn Fn(&str) + Send + Sync>,
        jitter_permille: i32,
    ) -> Result<Self, String> {
        let node = store
            .recover()
            .map_err(text)?
            .ok_or("this host is not enrolled: run `kbf-updater enroll` first")?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(text)?;
        let due = first_attempt(now(), node.renew_after, jitter_permille);
        Ok(Self {
            url,
            store,
            runtime,
            now,
            log,
            jitter: jitter_permille,
            schedule: Mutex::new(Schedule {
                due,
                backoff: Backoff::new(),
            }),
        })
    }

    /// One renewal: dial the frontend under the pin the host holds and send the request.
    fn attempt(&self, now: i64) -> Result<Node, String> {
        self.runtime.block_on(async {
            let node = self
                .store
                .recover()
                .map_err(text)?
                .ok_or("this host is not enrolled")?;
            let pin = node.pin().map_err(text)?;
            let frontend = connect(&self.url, Anchor::Pin(pin)).await.map_err(text)?;
            renew(&frontend, &self.store, now).await.map_err(text)
        })
    }

    /// A failed attempt, in the log; below a day to expiry it is an error.
    fn warn(&self, why: &str, now: i64) {
        let expiring = self
            .store
            .load()
            .ok()
            .flatten()
            .filter(|node| near_expiry(node, now));
        match expiring {
            Some(node) => (self.log)(&format!(
                "cannot renew the certificate, which expires in {} s: {why}",
                node.earliest_not_after() - now
            )),
            None => (self.log)(&format!("cannot renew the certificate; will retry: {why}")),
        }
    }
}

/// How long to wait for `seconds`: at least one (a frontend that says "renew now" must not make
/// a loop that calls it as fast as it can answer), at most [`NAP_MAX_S`].
pub(super) fn nap(seconds: i64) -> Duration {
    Duration::from_secs(u64::try_from(seconds).unwrap_or(0).clamp(1, NAP_MAX_S))
}

impl Renewer for FrontendRenewer {
    fn round(&self) -> Duration {
        let now = (self.now)();
        let mut schedule = self.schedule.lock().unwrap_or_else(PoisonError::into_inner);
        if now < schedule.due {
            return nap(schedule.due - now);
        }
        match self.attempt(now) {
            Ok(node) => {
                schedule.backoff.succeeded();
                schedule.due = first_attempt(now, node.renew_after, self.jitter);
                (self.log)(&format!(
                    "renewed the certificate; it is valid until unix time {}",
                    node.earliest_not_after()
                ));
            }
            Err(why) => {
                schedule.due = now + schedule.backoff.failed();
                self.warn(&why, now);
            }
        }
        nap(schedule.due - now)
    }
}
