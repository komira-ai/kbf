//! The janitor's sweep of orphans under `cas/b/`.
//!
//! A task writes its packs (`-<k>`) and large objects (`-L<i>`) to `cas/b/<build>/<nonce>-<slot>`
//! through presigned writes. When the
//! pack is rejected, or the run fails after the write, nothing records it and nothing reads it:
//! it is an orphan. [`Sweeper::sweep`] lists the prefix and deletes the orphans that are older
//! than the grace period. Every delete is gated four ways: by the prefix (the listing is asked
//! for [`SWEPT_PREFIX`], and each key it returns is checked to be under it), by the age the key
//! itself says, by the set of keys under verification, and by the pack index, which is asked
//! again for the one key just before the delete. Between pages the sweep waits
//! [`SweepConfig::pause`], which bounds how fast it lists and deletes.

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

/// The longest a presigned write of a task's key is good for: 4 hours. A key is never written
/// after it is this old.
pub const MAX_KEY_LIFE: Duration = Duration::from_secs(4 * 60 * 60);

/// The time the ingest of a run is given after the last write its key allows.
pub const INGEST_ALLOWANCE: Duration = Duration::from_secs(60 * 60);

/// The least a key's age may be for the janitor to delete it, and the age from which an ingest
/// refuses it: the longest a key can be written ([`MAX_KEY_LIFE`]) and the ingest after it
/// ([`INGEST_ALLOWANCE`]). One constant for both sides, so a key an ingest accepts is never one
/// the janitor may delete.
pub const MIN_GRACE: Duration =
    Duration::from_secs(MAX_KEY_LIFE.as_secs() + INGEST_ALLOWANCE.as_secs());

/// How a sweep runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SweepConfig {
    /// A key younger than this is kept.
    pub grace: Duration,
    /// Keys per page of the listing, and so per question to the index.
    pub page: PageSize,
    /// Log and count what would be deleted, and delete nothing.
    pub dry_run: bool,
    /// How long the sweep waits after each page before it lists the next: with `page`, the
    /// bound on how fast a sweep lists and deletes. Zero does not wait.
    pub pause: Duration,
    /// The sweep stops, with [`SweepError::Failing`], after this many deletes in a row that the
    /// store refused or could not answer.
    pub max_consecutive_failures: usize,
}

impl Default for SweepConfig {
    fn default() -> Self {
        Self {
            grace: DEFAULT_GRACE,
            page: PageSize::MAX,
            dry_run: false,
            pause: Duration::ZERO,
            max_consecutive_failures: 5,
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

/// What a claim on a key holds until it is dropped.
pub type Claim = Box<dyn Send>;

/// Claims a key for its delete, or says it cannot.
pub type ClaimOf = Box<dyn Fn(&str) -> Option<Claim> + Send + Sync>;

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
    /// Claims the key for its delete, atomically with the check that nothing is verifying it:
    /// `None` when something is. The claim is held through the index's last answer and the
    /// delete, and while it is held nothing may begin to verify the key.
    pub claim: ClaimOf,
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
    /// Keys the listing returned that are not under [`SWEPT_PREFIX`] (a store that ignored the
    /// prefix): never looked at further, never deleted.
    pub outside: usize,
    /// Objects deleted.
    pub deleted: usize,
    /// Objects a dry run would have deleted.
    pub would_delete: usize,
    /// Deletes the store refused; the next sweep tries again.
    pub delete_failed: usize,
    /// This process stopped leading part way through, and the sweep stopped there.
    pub abandoned: bool,
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
    /// The store answered a delete with 401 (a bad credential) or 403: the key may not delete under the prefix, and
    /// no other delete of the sweep would be allowed either.
    #[error("sweep: the store refused to let this key delete under {SWEPT_PREFIX}: {0}")]
    Denied(#[source] ObjectStoreError),
    /// The store refused or could not answer this many deletes in a row.
    #[error("sweep: {failures} deletes in a row failed; the last: {last}")]
    Failing {
        /// The deletes in a row.
        failures: usize,
        /// The last error.
        #[source]
        last: ObjectStoreError,
    },
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
    /// [`SweepError`]: the sweep stops, and what it deleted before stays deleted. A caller that
    /// wants what the sweep did before it stopped uses [`Sweeper::sweep_into`].
    pub async fn sweep(&self, now: SystemTime) -> Result<SweepReport, SweepError> {
        let mut report = SweepReport::default();
        self.sweep_into(now, &mut report).await?;
        Ok(report)
    }

    /// One sweep, judging every age against `now`, counting into `report` as it goes, so that
    /// when the sweep stops with an error `report` says what it did before.
    ///
    /// # Errors
    /// [`SweepError::List`], [`SweepError::Index`], [`SweepError::Denied`] or
    /// [`SweepError::Failing`]: the sweep stops, and what it deleted before stays deleted.
    pub async fn sweep_into(
        &self,
        now: SystemTime,
        report: &mut SweepReport,
    ) -> Result<(), SweepError> {
        let prefix = KeyPrefix::new(SWEPT_PREFIX).expect("the swept prefix is a valid prefix");
        let mut after: Option<ListToken> = None;
        let mut failures = 0;
        loop {
            // Asked before each page and before each delete: a server that stops leading stops
            // deleting at once, and says so if it had begun.
            if !(self.gates.is_leader)() {
                report.abandoned = report.led;
                return Ok(());
            }
            report.led = true;
            let page = self
                .store
                .list(&prefix, after.as_ref(), self.config.page)
                .await
                .map_err(SweepError::List)?;
            report.listed += page.objects.len();
            let mut candidates = Vec::new();
            for object in &page.objects {
                let key = object.key.as_str();
                // The listing is asked for this prefix only; a store that answers with more is
                // not trusted to have kept to it, and nothing else is ever a candidate.
                if !key.starts_with(SWEPT_PREFIX) {
                    tracing::warn!(%key, "the listing of cas/b/ returned a key outside it: kept");
                    report.outside += 1;
                    continue;
                }
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
            self.judge(candidates, report, &mut failures).await?;
            if report.abandoned {
                return Ok(());
            }
            match page.next {
                Some(token) => after = Some(token),
                None => return Ok(()),
            }
            tokio::time::sleep(self.config.pause).await;
        }
    }

    /// Deletes the keys among `candidates` that the index does not record.
    async fn judge(
        &self,
        candidates: Vec<ObjectKey>,
        report: &mut SweepReport,
        failures: &mut usize,
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
            // From here to the end of the delete nothing may begin to verify the key (and
            // if something already does, the key is kept): the claim is taken after the last
            // answer of the page and held through the index's answer for this key and the delete.
            let Some(_claim) = (self.gates.claim)(&key.to_string()) else {
                report.verifying += 1;
                continue;
            };
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
            // Asked last, after the claim and the index's last answer and with nothing between
            // it and the delete: a server that stopped leading while it was waiting on either
            // deletes nothing (the claim is let go as this returns).
            if !(self.gates.is_leader)() {
                report.abandoned = true;
                return Ok(());
            }
            match self.store.delete(&key).await {
                Ok(()) => {
                    tracing::info!(%key, "deleted an orphan pack");
                    report.deleted += 1;
                    *failures = 0;
                }
                Err(
                    error @ ObjectStoreError::Service {
                        status: 401 | 403, ..
                    },
                ) => {
                    return Err(SweepError::Denied(error));
                }
                Err(error) => {
                    tracing::warn!(%key, %error, "could not delete an orphan pack");
                    report.delete_failed += 1;
                    *failures += 1;
                    if *failures >= self.config.max_consecutive_failures {
                        let failures = *failures;
                        return Err(SweepError::Failing {
                            failures,
                            last: error,
                        });
                    }
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
