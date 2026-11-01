//! What the heartbeat says about the host: the one record the heartbeat thread reads and the
//! thread that updates writes.
//!
//! It is the platform, the version the host runs, its health, the last update it ran, and the
//! target of the update running now (`docs/design/updates.md`, section 2). Who the host is
//! (its node id, its role) is not in it: the coordinator reads that from the certificate.

use std::sync::{Arc, Mutex, MutexGuard};

use kbf_types::ReleasePlatform;

use crate::version::Version;
use crate::wire::{Health, Heartbeat, ServiceBeat, UpdateResult};

/// The platform this build runs on, when kbf has releases for it.
pub fn host_platform() -> Option<ReleasePlatform> {
    platform_of(std::env::consts::OS, std::env::consts::ARCH)
}

/// The platform of an operating system and CPU as Rust names them (`linux`, `macos`; `x86_64`,
/// `aarch64`), when kbf has releases for it.
pub fn platform_of(os: &str, arch: &str) -> Option<ReleasePlatform> {
    let arch = if arch == "aarch64" { "arm64" } else { arch };
    ReleasePlatform::from_report(os, arch)
}

/// What the host last knew about itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub platform: ReleasePlatform,
    pub version: Version,
    pub health: Health,
    pub last_update: Option<UpdateResult>,
    pub updating: Option<Version>,
}

impl Status {
    /// What a host knows before it has asked its helper: `placeholder` for a version it cannot
    /// name yet, and not healthy.
    pub fn unknown(platform: ReleasePlatform, placeholder: Version) -> Self {
        Self {
            platform,
            version: placeholder,
            health: Health::Unhealthy,
            last_update: None,
            updating: None,
        }
    }

    /// The heartbeat that says it.
    pub fn heartbeat(&self) -> Heartbeat {
        Heartbeat {
            platform: self.platform,
            version: self.version.clone(),
            health: self.health,
            last_update: self.last_update.clone(),
            updating: self.updating.clone(),
            services: None,
            agent_features: Vec::new(),
            step: None,
            commit: None,
            step_secs: Default::default(),
            promotion_pending: false,
            deferred: None,
        }
    }

    /// What the host says of the service `name`, when this is that service's status.
    pub fn service_beat(&self, name: &str) -> ServiceBeat {
        ServiceBeat {
            name: name.to_owned(),
            version: self.version.clone(),
            health: self.health,
            last_update: self.last_update.clone(),
            updating: self.updating.clone(),
            step: None,
            commit: None,
            step_secs: Default::default(),
            promotion_pending: false,
            deferred: None,
        }
    }
}

/// The status the threads share.
#[derive(Clone, Debug)]
pub struct Shared(Arc<Mutex<Status>>);

impl Shared {
    pub fn new(status: Status) -> Self {
        Self(Arc::new(Mutex::new(status)))
    }

    fn lock(&self) -> MutexGuard<'_, Status> {
        // A thread that panicked while holding it left a status that is still a status.
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A copy of the status now.
    pub fn get(&self) -> Status {
        self.lock().clone()
    }

    /// Change the status.
    pub fn update(&self, change: impl FnOnce(&mut Status)) {
        change(&mut self.lock());
    }
}

/// The version text of a release whose version this host was never told: its commit alone, in
/// the form a version carries it (`0.1.0+g<12 hex>`), which names the commit and nothing else.
pub fn version_of_commit(commit: &str) -> Option<Version> {
    let short = commit.get(..12)?;
    Version::parse(&format!("0.1.0+g{short}")).ok()
}
