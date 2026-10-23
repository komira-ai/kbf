//! The policy row of the desired release.

use std::time::Duration;

/// The `update/policy` row (`docs/design/pull-updates.md` section 6.2) as the operator API
/// shows it. [`Policy::default`] is what a policy never written means. Of everything here
/// only `paused` is written (by [`super::Rows::set_paused`]) and acted on; the other settings
/// and the limits are the ones older builds' node step used, kept so that a row they wrote
/// reads back and `GET /v1/update` shows it. The row carries a limit only if it differs from
/// the default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// Hosts were started for the desired release without a click. True by default.
    pub auto_nodes: bool,
    /// As `auto_nodes`, for the frontend's own update.
    pub auto_frontend: bool,
    /// No run is started or continued while this is set ([`super::Rows::set_paused`]). False
    /// by default.
    pub paused: bool,
    /// New work went only to hosts at the target release.
    pub new_work_only_to_updated: bool,
    /// The operator accepted failing builds during a run.
    pub accept_failing_builds: bool,
    /// A member row's time to live (15 s by default).
    pub ttl: Duration,
    /// How long a `Down` member stayed in the map (300 s by default).
    pub dwell: Duration,
    /// How long to wait for a node to go idle before cordoning it (120 s by default).
    pub idle_wait: Duration,
    /// How many other hosts of a platform had to stay serving (0 by default, no floor).
    pub min_serving: usize,
    /// How many daemon-only hosts and Macs could be in a step at once (1 by default).
    pub max_hosts_out: usize,
    /// How long to wait for leases to end by themselves (15 minutes by default).
    pub lease_limit: Duration,
    /// The same on a Mac (60 minutes by default).
    pub mac_lease_limit: Duration,
    /// How long to wait for open write sessions to reach zero (60 s by default).
    pub write_limit: Duration,
    /// How long to wait for the helper to report an activation (5 minutes by default).
    pub activate_limit: Duration,
    /// How long a claim, a cordon, a return to service or a confirm could take to show (120 s
    /// by default).
    pub health_limit: Duration,
    /// How long a rebuilding node could take; `None` (the default) is without a bound.
    pub rebuild_limit: Option<Duration>,
    /// How long the debts of a node's downtime could take to settle (5 minutes by default).
    pub repair_wait: Duration,
    /// After this long without an idle Mac the least-loaded one was cordoned; `None` (the
    /// default) never forced a Mac.
    pub mac_force_after: Option<Duration>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            auto_nodes: true,
            auto_frontend: false,
            paused: false,
            new_work_only_to_updated: false,
            accept_failing_builds: false,
            ttl: Duration::from_secs(15),
            dwell: Duration::from_secs(300),
            idle_wait: Duration::from_secs(120),
            min_serving: 0,
            max_hosts_out: 1,
            lease_limit: Duration::from_secs(15 * 60),
            mac_lease_limit: Duration::from_secs(60 * 60),
            write_limit: Duration::from_secs(60),
            activate_limit: Duration::from_secs(5 * 60),
            health_limit: Duration::from_secs(120),
            rebuild_limit: None,
            repair_wait: Duration::from_secs(5 * 60),
            mac_force_after: None,
        }
    }
}
