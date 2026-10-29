//! The janitor's sweep of orphans under `cas/b/`.
//!
//! A task writes its packs (`-<k>`) and large objects (`-L<i>`) to `cas/b/<build>/<nonce>-<slot>`
//! through presigned writes. When the
//! pack is rejected, or the run fails after the write, nothing records it and nothing reads it:
//! it is an orphan. [`Sweeper::sweep`] lists the prefix and deletes the orphans that are older
//! than the grace period. Every delete is gated four ways: by the prefix, by the age the key
//! itself says, by the set of keys under verification, and by the pack index, which is asked
//! again for the one key just before the delete.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use kbf_objstore::{KeyPrefix, ListToken, ObjectKey, ObjectStore, ObjectStoreError, PageSize};
use kbf_pack_index::{PackIndex, PackIndexError};
use kbf_task_pack::TaskPackKey;

use crate::blocking::offload;

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

/// The age a task pack key says: the start of the minute it was minted in, whatever its slot
/// (`-<k>` or `-L<i>`). `None` for a key that is not a task pack key. This is the usual
/// [`SweepGates::age_of`].
#[must_use]
pub fn task_key_age(key: &str) -> Option<SystemTime> {
    let key = TaskPackKey::parse(key).ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(key.minted_unix_secs()))
}

/// When a key was minted, read from the key itself; `None` when the key does not say.
pub type AgeOf = Box<dyn Fn(&str) -> Option<SystemTime> + Send + Sync>;

/// The three questions a sweep asks of its caller.
pub struct SweepGates {
    /// When the key was minted, read from the key; `None` for a key that does not say.
    pub age_of: AgeOf,
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
        let mut report = SweepReport::default();
        if !(self.gates.is_leader)() {
            return Ok(report);
        }
        report.led = true;
        let prefix = KeyPrefix::new(SWEPT_PREFIX).expect("the swept prefix is a valid prefix");
        let mut after: Option<ListToken> = None;
        loop {
            let page = self
                .store
                .list(&prefix, after.as_ref(), self.config.page)
                .await
                .map_err(SweepError::List)?;
            report.listed += page.objects.len();
            let mut candidates = Vec::new();
            for object in &page.objects {
                let key = object.key.as_str();
                let Some(born) = (self.gates.age_of)(key) else {
                    tracing::warn!(%key, "a key under cas/b/ does not say its age: kept");
                    report.strays.push(key.to_owned());
                    continue;
                };
                // A key minted in the future (clock skew) is as young as it can be.
                let age = now.duration_since(born).unwrap_or(Duration::ZERO);
                if age < self.config.grace {
                    report.young += 1;
                } else if (self.gates.verifying)(key) {
                    report.verifying += 1;
                } else {
                    candidates.push(object.key.clone());
                }
            }
            self.judge(candidates, &mut report).await?;
            match page.next {
                Some(token) => after = Some(token),
                None => return Ok(report),
            }
        }
    }

    /// Deletes the keys among `candidates` that the index does not record.
    async fn judge(
        &self,
        candidates: Vec<ObjectKey>,
        report: &mut SweepReport,
    ) -> Result<(), SweepError> {
        if candidates.is_empty() {
            return Ok(());
        }
        let asked = candidates.len();
        let names = candidates.iter().map(ToString::to_string).collect();
        let orphans = self.unreferenced(names).await?;
        report.referenced += asked.saturating_sub(orphans.len());
        for key in candidates {
            if !orphans.contains(&key.to_string()) {
                continue;
            }
            // The page was judged a moment ago: a pack may have been recorded since.
            if self.unreferenced(vec![key.to_string()]).await?.is_empty() {
                report.referenced += 1;
                continue;
            }
            if self.config.dry_run {
                tracing::info!(%key, "an orphan pack would be deleted");
                report.would_delete += 1;
                continue;
            }
            match self.store.delete(&key).await {
                Ok(()) => {
                    tracing::info!(%key, "deleted an orphan pack");
                    report.deleted += 1;
                }
                Err(error) => {
                    tracing::warn!(%key, %error, "could not delete an orphan pack");
                    report.delete_failed += 1;
                }
            }
        }
        Ok(())
    }

    async fn unreferenced(&self, keys: Vec<String>) -> Result<Vec<String>, SweepError> {
        let index = Arc::clone(&self.index);
        offload(move || index.unreferenced_keys(&keys))
            .await
            .map_err(SweepError::Index)
    }
}
