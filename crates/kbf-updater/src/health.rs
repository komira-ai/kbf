//! The real [`HealthCheck`]: whether a host runs the target, by its role.
//!
//! - the frontend: [`FrontendHealth`], kbf-proxy's `GET /v1/proxy/swap`;
//! - a storage or daemon host: [`NodeHealth`], the frontend's operator API.
//!
//! [`HostHealth`] holds one of each and sends a host to the right one. This module also holds
//! what both share: the time of the install call, and the one way a version is tied to a
//! commit.
//!
//! # A version and its commit
//!
//! The roll names a package [`Version`]; the hosts report commits. The link is the version
//! itself: a package of the series is `0.1.<seq>+g<sha12>-<run>` (`xtask`'s `pkg_version`),
//! so it carries the first 12 hex digits of its commit ([`Version::commit`]). A reported
//! commit matches when it begins with those digits ([`names`]); a frontend says 40 digits, a
//! storage node 40 or 12. A version that names no commit cannot be verified against a host,
//! so a host is not called healthy by it (48 bits of prefix: the target is chosen by a
//! trusted workflow, not by an attacker who could grind a collision).
//!
//! # The install call
//!
//! A frontend that already ran the target (an earlier roll) still shows that swap as `done`,
//! and a daemon that has not restarted yet is still connected. So each adapter notes the time
//! of the install call ([`InstallTimes`]) and accepts only what happened after it. After an
//! updater restart in the middle of a roll that time is unknown (it is not in `roll.json`),
//! and the check is not made: the other conditions still hold.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::frontend_health::FrontendHealth;
use crate::inventory::{Host, Role};
use crate::node_health::NodeHealth;
use crate::ports::{Health, HealthCheck, PortError};
use crate::version::Version;

/// Milliseconds since the Unix epoch. Injected so that a test sets the install time; the
/// updater runs on the frontend host, so this is the clock the proxy and the server stamp
/// with.
pub type WallClock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The system's wall clock.
pub fn system_wall_clock() -> WallClock {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    })
}

/// When the roll called the installer for each host, by alias; the last call of a host wins.
pub struct InstallTimes {
    now: WallClock,
    at: Mutex<BTreeMap<String, u64>>,
}

impl InstallTimes {
    pub fn new(now: WallClock) -> InstallTimes {
        InstallTimes {
            now,
            at: Mutex::new(BTreeMap::new()),
        }
    }

    /// The installer is about to be called for `alias`.
    pub fn note(&self, alias: &str) {
        let now = (self.now)();
        self.at
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(alias.to_owned(), now);
    }

    /// When the installer was last called for `alias`, if this process did.
    pub fn at(&self, alias: &str) -> Option<u64> {
        self.at
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(alias)
            .copied()
    }
}

/// Whether `observed`, a commit a host reported (40 hex digits, 12, or `unknown`), is the
/// commit that begins with `commit` (hex digits, from [`Version::commit`]). A report shorter
/// than `commit` names less than the target and so is not it.
pub fn names(observed: &str, commit: &str) -> bool {
    observed
        .get(..commit.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(commit))
}

/// The commit `target` names, or the answer that says the host cannot be judged by it.
pub fn commit_of(target: &Version) -> Result<&str, Health> {
    target.commit().ok_or_else(|| {
        Health::Abandoned(format!(
            "the version {target} names no commit (+g<12 hex digits>), so no host can be shown to run it"
        ))
    })
}

/// The health of every host: the frontend through the proxy, the rest through the operator
/// API.
pub struct HostHealth {
    frontend: FrontendHealth,
    nodes: NodeHealth,
}

impl HostHealth {
    pub fn new(frontend: FrontendHealth, nodes: NodeHealth) -> HostHealth {
        HostHealth { frontend, nodes }
    }

    fn for_role(&self, role: Role) -> &dyn HealthCheck {
        match role {
            Role::Frontend => &self.frontend,
            Role::Storage | Role::Daemon => &self.nodes,
        }
    }
}

impl HealthCheck for HostHealth {
    fn installing(&self, host: &Host) {
        self.for_role(host.role).installing(host);
    }

    fn check(&self, host: &Host, target: &Version) -> Result<Health, PortError> {
        self.for_role(host.role).check(host, target)
    }
}
