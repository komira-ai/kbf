//! Names: an estimator key as a profile name, a worker as a host.

use std::sync::atomic::AtomicU64;

use js_types::HostId;
use js_usage::ProfileKey;
use kbf_estimator::Key;
use kbf_types::WorkerId;

/// Placeholder: every key has one name.
#[must_use]
pub fn profile_name(key: &Key) -> ProfileKey {
    ProfileKey::parse("0").expect("a valid profile key")
}

/// Placeholder: every worker is `w-0`, and nothing is counted.
#[derive(Default)]
pub struct HostNames {
    renamed: AtomicU64,
}

impl HostNames {
    /// A namer that has renamed nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The host named for `worker`.
    pub fn host_name(&self, worker: &WorkerId) -> HostId {
        HostId::new("w-0").expect("a valid host id")
    }

    /// How many calls so far gave a worker a name other than its own.
    #[must_use]
    pub fn renamed(&self) -> u64 {
        0
    }
}
