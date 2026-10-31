//! Deferrals: an update the host declined for a reason about its moment, not the release
//! (`docs/design/host-updates.md`, section 3.8).
//!
//! STUB: behaviour follows the tests.

use std::collections::BTreeMap;
use std::time::Duration;

use kbf_types::NodeId;
use kbf_updater::version::Version;
use kbf_updater::wire::Heartbeat;
use serde::Deserialize;

/// How long a deferred host is left alone before it is ordered again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeferBackoff {
    /// The wait after the first deferral of a version, in seconds.
    pub initial_secs: u64,
    /// What each further deferral of the same version multiplies the wait by.
    pub factor: u32,
    /// The longest wait, in seconds.
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
    /// The wait after the `episodes`-th deferral of one version.
    #[must_use]
    pub fn wait(&self, episodes: u32, retry_after_secs: u64) -> Duration {
        let _ = (episodes, retry_after_secs);
        Duration::ZERO
    }
}

/// The open deferrals, by host and service. In memory only.
#[derive(Clone, Debug, Default)]
pub(crate) struct Defers {
    open: BTreeMap<(NodeId, String), ()>,
}

impl Defers {
    pub(crate) fn observe(
        &mut self,
        key: &(NodeId, String),
        beat: &Heartbeat,
        ended_order: bool,
        now: Duration,
    ) {
        let _ = (key, beat, ended_order, now);
        let _ = &self.open;
    }

    pub(crate) fn until(
        &self,
        key: &(NodeId, String),
        target: &Version,
        backoff: &DeferBackoff,
    ) -> Option<Duration> {
        let _ = (key, target, backoff);
        None
    }
}
