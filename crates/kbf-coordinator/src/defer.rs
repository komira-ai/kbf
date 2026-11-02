//! Deferrals: an update the host declined for a reason about its moment, not the release
//! (`docs/design/host-updates.md`, section 3.8).
//!
//! A host that says it declined an update to a version (a [`DeferredReport`] in its
//! heartbeat) is left alone for a wait, holds no slot and does not make its service
//! `blocked`; at the end of the wait it is ordered again like any other host. A deferral
//! never cuts work: it only delays the offer of one version to one host, and a new desired
//! version lifts it at once.
//!
//! A host declining several orders in a row waits longer each time: the wait of the n-th
//! episode is `initial_secs * factor^(n-1)` capped at `max_secs`, or the report's own
//! `retry_after_secs` if that is larger ([`DeferBackoff`], the `defer_backoff` of a service
//! definition, default 300 s, doubling, capped at 3600 s). An episode opens when a host that
//! was ordered reports the deferral, or the first time a report is seen; the same report
//! repeated in later heartbeats is the same episode and moves nothing.
//!
//! The episodes are kept in memory only: after a restart the first report starts the count
//! again.

use std::collections::BTreeMap;
use std::time::Duration;

use kbf_types::NodeId;
use kbf_updater::version::Version;
use kbf_updater::wire::{DeferredReport, Heartbeat};
use serde::Deserialize;

/// The longest cap a definition may set: a week.
pub const MAX_BACKOFF_SECS: u64 = 7 * 24 * 3600;

/// The largest factor a definition may set.
pub const MAX_FACTOR: u32 = 16;

/// How long a deferred host is left alone before it is ordered again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeferBackoff {
    /// The wait after the first deferral of a version, in seconds.
    pub initial_secs: u64,
    /// What each further deferral of the same version multiplies the wait by.
    pub factor: u32,
    /// The longest the backoff grows to, in seconds. It does not cap the host's own
    /// `retry_after_secs`, which the agent clamps itself (3600 s): keep this at or above 3600
    /// unless a service should be re-offered sooner than an agent can ask to be left alone,
    /// and then only the host's hint, not this number, decides how long that host waits.
    pub max_secs: u64,
}

impl Default for DeferBackoff {
    fn default() -> Self {
        Self {
            initial_secs: 300,
            factor: 2,
            max_secs: 3600,
        }
    }
}

impl DeferBackoff {
    /// The wait after the `episodes`-th deferral of one version, when the host asked for
    /// `retry_after_secs`: the grown backoff, capped at `max_secs`, or the host's number if
    /// larger. Only the backoff is capped: ordering a host before the time it asked for just
    /// makes it decline again.
    #[must_use]
    pub fn wait(&self, episodes: u32, retry_after_secs: u64) -> Duration {
        let growth = u64::from(self.factor).saturating_pow(episodes.saturating_sub(1));
        let grown = self.initial_secs.saturating_mul(growth);
        Duration::from_secs(grown.min(self.max_secs).max(retry_after_secs))
    }

    /// Why the values cannot be used, if they cannot: an initial wait of zero would not
    /// wait, a factor of zero or over [`MAX_FACTOR`] is a typo, and the cap must hold the
    /// initial wait and stay within [`MAX_BACKOFF_SECS`].
    pub(crate) fn check(&self) -> Result<(), String> {
        if self.initial_secs == 0 {
            Err("initial_secs is 0".to_owned())
        } else if !(1..=MAX_FACTOR).contains(&self.factor) {
            Err(format!("factor is not 1 to {MAX_FACTOR}"))
        } else if self.max_secs < self.initial_secs {
            Err("max_secs is under initial_secs".to_owned())
        } else if self.max_secs > MAX_BACKOFF_SECS {
            Err(format!("max_secs is over {MAX_BACKOFF_SECS}"))
        } else {
            Ok(())
        }
    }
}

/// One host's open deferral of one service.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Defer {
    target: Version,
    /// When the current episode opened.
    opened: Duration,
    /// How many episodes of this target the host has had.
    episodes: u32,
    /// What the host asked for in the report that opened the episode.
    retry_after_secs: u64,
}

/// The open deferrals, by host and service.
#[derive(Clone, Debug, Default)]
pub(crate) struct Defers {
    open: BTreeMap<(NodeId, String), Defer>,
}

impl Defers {
    /// Notes the heartbeat `beat` of the host and service `key` at `now`. `ordered` is true
    /// when the beat's deferral ended an order the coordinator had given: the host was
    /// offered the version again and declined again, a new episode. A host that is updating
    /// is no longer deferring.
    pub(crate) fn observe(
        &mut self,
        key: &(NodeId, String),
        beat: &Heartbeat,
        ordered: bool,
        now: Duration,
    ) {
        if beat.updating.is_some() {
            self.open.remove(key);
            return;
        }
        let Some(report) = &beat.deferred else {
            return;
        };
        match self.open.get_mut(key) {
            Some(open) if open.target == report.target => {
                if ordered {
                    open.episodes = open.episodes.saturating_add(1);
                    open.opened = now;
                    open.retry_after_secs = report.retry_after_secs;
                }
            }
            _ => {
                self.open.insert(key.clone(), Defer::first(report, now));
            }
        }
    }

    /// When the wait of the host and service `key` ends, if it declined `target`.
    pub(crate) fn until(
        &self,
        key: &(NodeId, String),
        target: &Version,
        backoff: &DeferBackoff,
    ) -> Option<Duration> {
        let open = self.open.get(key).filter(|d| d.target == *target)?;
        Some(
            open.opened
                .saturating_add(backoff.wait(open.episodes, open.retry_after_secs)),
        )
    }
}

impl Defer {
    fn first(report: &DeferredReport, now: Duration) -> Self {
        Self {
            target: report.target.clone(),
            opened: now,
            episodes: 1,
            retry_after_secs: report.retry_after_secs,
        }
    }
}
