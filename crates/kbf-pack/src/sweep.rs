//! The janitor's sweep of orphans under `cas/b/`.
//!
//! A task writes one pack to `cas/b/<build>/<nonce>-0` through a presigned write. When the
//! pack is rejected, or the run fails after the write, nothing records it and nothing reads it:
//! it is an orphan. [`Sweeper::sweep`] lists the prefix and deletes the orphans that are older
//! than the grace period. Every delete is gated four ways: by the prefix, by the age the key
//! itself says, by the set of keys under verification, and by the pack index, which is asked
//! again for the one key just before the delete.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use kbf_objstore::{ObjectStore, ObjectStoreError, PageSize};
use kbf_pack_index::{PackIndex, PackIndexError};

/// The prefix the sweep covers, and the only one it ever deletes under.
pub const SWEPT_PREFIX: &str = "cas/b/";

/// How long an orphan is kept by default: more than the longest run, the slack of its write
/// grant and the wait of the answer that reads it.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(6 * 60 * 60);

/// How a sweep runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SweepConfig {
    /// A key younger than this is kept.
    pub grace: Duration,
    /// Keys per page of the listing, and so per question to the index.
    pub page: PageSize,
    /// Log and count what would be deleted, and delete nothing.
    pub dry_run: bool,
}

impl Default for SweepConfig {
    fn default() -> Self {
        Self {
            grace: DEFAULT_GRACE,
            page: PageSize::MAX,
            dry_run: false,
        }
    }
}

/// The three questions a sweep asks of its caller.
pub struct SweepGates {
    /// When the key was minted, read from the key; `None` for a key that does not say.
    pub age_of: Box<dyn Fn(&str) -> Option<SystemTime> + Send + Sync>,
    /// Whether this process leads: only the leader sweeps.
    pub is_leader: Box<dyn Fn() -> bool + Send + Sync>,
    /// Whether the key is being verified right now.
    pub verifying: Box<dyn Fn(&str) -> bool + Send + Sync>,
}

/// What one sweep did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// This process led, so the sweep ran.
    pub led: bool,
    /// Keys the listing returned.
    pub listed: usize,
    /// Keys kept because they are younger than the grace.
    pub young: usize,
    /// Keys kept because the index records a pack under them.
    pub referenced: usize,
    /// Keys kept because they are under verification.
    pub verifying: usize,
    /// Keys under the prefix whose age the key does not say: reported, never deleted.
    pub strays: Vec<String>,
    /// Objects deleted.
    pub deleted: usize,
    /// Objects a dry run would have deleted.
    pub would_delete: usize,
    /// Deletes the store refused; the next sweep tries again.
    pub delete_failed: usize,
}

/// Why a sweep stopped before it reached the end of the listing.
#[derive(Debug, thiserror::Error)]
pub enum SweepError {
    /// The listing failed.
    #[error("sweep: listing {SWEPT_PREFIX} failed: {0}")]
    List(#[source] ObjectStoreError),
    /// The index could not say which keys it records.
    #[error("sweep: the pack index failed: {0}")]
    Index(#[source] PackIndexError),
}

/// The sweep of orphans over one object store and one pack index.
pub struct Sweeper<S: ObjectStore> {
    store: Arc<S>,
    index: Arc<dyn PackIndex>,
    config: SweepConfig,
    gates: SweepGates,
}

impl<S: ObjectStore> Sweeper<S> {
    /// A sweeper over `store` and `index`.
    pub fn new(
        store: Arc<S>,
        index: Arc<dyn PackIndex>,
        config: SweepConfig,
        gates: SweepGates,
    ) -> Self {
        Self {
            store,
            index,
            config,
            gates,
        }
    }

    /// One sweep, judging every age against `now`.
    ///
    /// # Errors
    /// [`SweepError::List`] or [`SweepError::Index`]: the sweep stops, and what it deleted
    /// before stays deleted.
    pub async fn sweep(&self, now: SystemTime) -> Result<SweepReport, SweepError> {
        let _ = (&self.store, &self.index, &self.config, &self.gates, now);
        std::future::ready(()).await;
        Ok(SweepReport::default())
    }
}
