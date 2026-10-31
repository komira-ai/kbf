//! Rolling a service back by itself: what the coordinator records about a version and what
//! it does when a release fails on enough hosts (`docs/design/host-updates.md`, sections 3.8
//! and 5.3).
//!
//! [`plan`] is a pure function of a service's definition, its desired state, the marks the
//! ops store holds for it ([`crate::ops::ServiceOps`]), the fleet and the time; it says what
//! to change and changes nothing. The coordinator applies the answer after every heartbeat
//! (`apply`).

use std::time::Duration;

use kbf_updater::version::Version;

use crate::desired::Desired;
use crate::fleet::Fleet;
use crate::ops::ServiceOps;
use crate::services::ServiceDef;

/// Whether the coordinator may roll a service back by itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Rollback {
    /// Only an operator or CI sets an older desired version.
    #[default]
    Manual,
    /// The coordinator sets the last good version when a release fails on enough hosts.
    Automatic,
}

/// How many hosts must fail an update to the desired version before an `automatic` service
/// rolls back: the larger of `min_hosts` and `percent` of the live hosts, rounded up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FailThreshold {
    /// The share of live hosts, 1 to 100.
    pub percent: u32,
    /// The fewest hosts, at least 1.
    pub min_hosts: u32,
}

impl Default for FailThreshold {
    /// A quarter of the hosts and at least two: two failed hosts is the smallest signal that
    /// is not one bad host.
    fn default() -> Self {
        Self {
            percent: 25,
            min_hosts: 2,
        }
    }
}

/// One thing [`plan`] says to do for a service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// Records `version` as the last good one.
    LastGood(Version),
    /// Forgets the last good version.
    ClearLastGood,
    /// Marks `version` bad: it is never ordered again until CI sets it by hand.
    MarkBad(Version),
    /// Sets the desired version to `version`, the other desired fields as they are.
    RollBackTo(Version),
}

/// What to change for `def` given its desired state `want`, the marks `marks` and the fleet
/// at `now`.
#[must_use]
pub fn plan(
    def: &ServiceDef,
    want: &Desired,
    marks: &ServiceOps,
    fleet: &Fleet,
    now: Duration,
) -> Vec<Change> {
    let _ = (def, want, marks, fleet, now);
    Vec::new()
}
