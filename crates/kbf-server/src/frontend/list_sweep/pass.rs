//! One pass of the collector of tree lists: the sidecars listed, each list's age and size
//! asked of the store, and the old lists no open operation names deleted with their sidecars
//! (or counted, in a dry run). The module docs of [`super`] say why each list is kept.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use kbf_objstore::s3::S3Store;
use kbf_objstore::{KeyPrefix, ListToken, ObjectKey, ObjectStore, ObjectStoreError, PageSize};
use kbf_pack_index::{PackIndex, PackIndexError};

use crate::executor::rows::{Rows, RowsError};

/// The prefix of the tree lists: `cas/l/<id>`, `<id>` the SHA-256 of the list in lower-case
/// hex (tierstore's `tree_list_key`).
pub const LIST_PREFIX: &str = "cas/l/";

/// The prefix of the lists' L2 sidecars: `cas/l2/<id>`, the id of the list it is the sidecar
/// of (tierstore-list's `sidecar_key`).
pub const SIDECAR_PREFIX: &str = "cas/l2/";

/// How old a list must be before it may be deleted: older than a day, by the time the store
/// says it was written. A list is needed while its jobs can still start, which is the
/// frontend's 600 s memo of a tree and an operation's later attempts, and an open operation's
/// row names the list of its attempt whatever its age.
pub const MIN_AGE: Duration = Duration::from_secs(86_400);

/// The ages the lists are counted by, in whole days: 0 to 6, and 7 or more in the last.
pub const AGE_DAYS: usize = 8;

/// How a pass runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PassConfig {
    /// Delete what may be deleted; `false` counts it and deletes nothing (a dry run).
    pub apply: bool,
    /// The sidecars listed at a time, and so the lists asked of the pack index at once.
    pub page: PageSize,
    /// The wait after each page before the next is listed.
    pub pause: Duration,
    /// The store calls (a HEAD or a delete) that may fail in a row before the pass stops.
    pub max_consecutive_failures: usize,
}

impl Default for PassConfig {
    fn default() -> Self {
        Self {
            apply: false,
            page: PageSize::MAX,
            pause: Duration::from_secs(1),
            max_consecutive_failures: 5,
        }
    }
}

/// The lists of one age, and their bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Aged {
    /// The lists.
    pub lists: u64,
    /// Their bytes.
    pub bytes: u64,
}

/// What a pass did, counted whether it ended or was stopped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PassReport {
    /// This frontend led the pass: it listed at least one page.
    pub led: bool,
    /// The pass reached the end of the listing.
    pub whole: bool,
    /// The sidecars the listings returned.
    pub sidecars: u64,
    /// Lists (or sidecars whose list is gone) a day old or younger, kept.
    pub young: u64,
    /// Old lists an open operation's row names, kept.
    pub named: u64,
    /// Old lists the pack index records a pack under (an object with the bytes of a list that
    /// is also a large blob of the CAS), kept.
    pub recorded: u64,
    /// Sidecar keys that name no list id (not 64 lower-case hex digits), kept.
    pub unparsed: u64,
    /// Lists deleted.
    pub deleted_lists: u64,
    /// Their bytes.
    pub deleted_bytes: u64,
    /// Sidecars deleted.
    pub deleted_sidecars: u64,
    /// Lists a dry run would have deleted.
    pub would_delete_lists: u64,
    /// Their bytes.
    pub would_delete_bytes: u64,
    /// Sidecars a dry run would have deleted.
    pub would_delete_sidecars: u64,
    /// Store calls that failed: a HEAD of a list, or a delete. The list is kept.
    pub failed: u64,
    /// The pass stopped part way because this frontend stopped leading.
    pub abandoned: bool,
    /// The lists the pass found, by their age in whole days ([`AGE_DAYS`]), and their bytes.
    pub by_day: [Aged; AGE_DAYS],
}

/// Why a pass stopped before the end of the listing. What it did until then is in its report.
#[derive(Debug, thiserror::Error)]
pub enum PassError {
    /// A listing of the sidecars failed.
    #[error("the listing of cas/l2/ failed: {0}")]
    List(ObjectStoreError),
    /// The pack index could not say which lists it records.
    #[error("the pack index failed: {0}")]
    Index(PackIndexError),
    /// The operations' rows could not all be read, so what they name is not known.
    #[error("the operations' rows could not be read: {0}")]
    Rows(RowsError),
    /// A blocking call of the index or the rows did not return (it panicked).
    #[error("a call of the {0} did not return")]
    Lost(&'static str),
    /// The store refused a call with 401 or 403: the key may not do it, for any list.
    #[error("the store refused a call of the collector's key: {0}")]
    Denied(ObjectStoreError),
    /// Store calls failed this many times in a row.
    #[error("{failures} store calls failed in a row, the last: {last}")]
    Failing {
        /// How many.
        failures: usize,
        /// The last error.
        last: ObjectStoreError,
    },
}

/// The stores a pass calls: one client per prefix, each signed with the tier store GC key of
/// that prefix (a GC key lists, heads and deletes under one prefix only).
pub struct Stores {
    /// Signed with the key of `cas/l/`: the HEAD and the delete of a list.
    pub lists: S3Store,
    /// Signed with the key of `cas/l2/`: the listing and the delete of a sidecar.
    pub sidecars: S3Store,
}

/// An old list (or an old sidecar whose list is gone) that the pass may delete, once the pack
/// index and the rows have been asked.
struct Candidate {
    /// `cas/l/<id>`.
    list: ObjectKey,
    /// The list's size, or `None` when the store has no list under the key.
    size: Option<u64>,
    /// `cas/l2/<id>`.
    sidecar: ObjectKey,
}

/// One pass over the stores, the index and the rows.
pub struct Pass {
    pub(super) stores: Stores,
    pub(super) index: Arc<dyn PackIndex>,
    pub(super) rows: Arc<Rows>,
    pub(super) config: PassConfig,
    pub(super) is_leader: Box<dyn Fn() -> bool + Send + Sync>,
}

/// The state of a pass that is running: its report, and the store calls failing in a row.
struct Running<'a> {
    report: &'a mut PassReport,
    failures: usize,
    max: usize,
}

impl Running<'_> {
    /// Counts a store call that failed: a 401 or 403 stops the pass, and so does the
    /// [`PassConfig::max_consecutive_failures`]th failure in a row.
    fn failed(&mut self, error: ObjectStoreError) -> Result<(), PassError> {
        if matches!(
            error,
            ObjectStoreError::Service {
                status: 401 | 403,
                ..
            }
        ) {
            return Err(PassError::Denied(error));
        }
        self.report.failed += 1;
        self.failures += 1;
        if self.failures >= self.max {
            return Err(PassError::Failing {
                failures: self.failures,
                last: error,
            });
        }
        Ok(())
    }
}

/// The list `sidecar` (`cas/l2/<id>`) is the sidecar of, `cas/l/<id>`; `None` when its name is
/// not a list id.
fn list_of(sidecar: &ObjectKey) -> Option<ObjectKey> {
    let id = sidecar.as_str().strip_prefix(SIDECAR_PREFIX)?;
    let hex = id.len() == 64 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    hex.then(|| ObjectKey::new(format!("{LIST_PREFIX}{id}")).ok())
        .flatten()
}

/// How long before `now` the time `millis` (since the epoch) is; none for a time not before it.
fn age(now: SystemTime, millis: u64) -> Duration {
    let written = SystemTime::UNIX_EPOCH + Duration::from_millis(millis);
    now.duration_since(written).unwrap_or_default()
}

/// The slot of [`PassReport::by_day`] for `age`.
fn day_of(age: Duration) -> usize {
    let days = age.as_secs() / 86_400;
    usize::try_from(days).map_or(AGE_DAYS - 1, |days| days.min(AGE_DAYS - 1))
}

/// Runs `work` on a blocking thread, as the index's and the rows' calls block.
async fn offload<T: Send + 'static>(
    what: &'static str,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, PassError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| PassError::Lost(what))
}

impl Pass {
    /// A pass over `stores` that asks `index` and `rows`, runs as `config` says, and lists a
    /// page or deletes a list only while `is_leader` says so.
    #[must_use]
    pub fn new(
        stores: Stores,
        index: Arc<dyn PackIndex>,
        rows: Arc<Rows>,
        config: PassConfig,
        is_leader: Box<dyn Fn() -> bool + Send + Sync>,
    ) -> Self {
        Self {
            stores,
            index,
            rows,
            config,
            is_leader,
        }
    }

    /// Whether the pass deletes (`false`: a dry run).
    #[must_use]
    pub fn applies(&self) -> bool {
        self.config.apply
    }

    /// Runs one pass, judging ages at `now`, into `report`.
    ///
    /// # Errors
    /// [`PassError`]: what stopped the pass; `report` holds what it did until then.
    pub async fn run(&self, now: SystemTime, report: &mut PassReport) -> Result<(), PassError> {
        let prefix = KeyPrefix::new(SIDECAR_PREFIX.to_owned()).map_err(|e| {
            PassError::List(ObjectStoreError::Protocol(format!("{SIDECAR_PREFIX}: {e}")))
        })?;
        let max = self.config.max_consecutive_failures;
        let mut running = Running {
            report,
            failures: 0,
            max,
        };
        let mut after: Option<ListToken> = None;
        loop {
            if !(self.is_leader)() {
                running.report.abandoned = running.report.led;
                return Ok(());
            }
            running.report.led = true;
            let page = self
                .stores
                .sidecars
                .list_stamped(&prefix, after.as_ref(), self.config.page);
            let page = page.await.map_err(PassError::List)?;
            let mut found = Vec::new();
            for sidecar in page.objects {
                running.report.sidecars += 1;
                let Some(list) = list_of(&sidecar.key) else {
                    running.report.unparsed += 1;
                    continue;
                };
                let (written, size) = match self.stores.lists.head(&list).await {
                    Ok(head) => (head.modified_millis, Some(head.size)),
                    Err(ObjectStoreError::NotFound(_)) => (sidecar.modified_millis, None),
                    Err(error) => {
                        running.failed(error)?;
                        continue;
                    }
                };
                running.failures = 0;
                let age = age(now, written);
                if let Some(size) = size {
                    let day = &mut running.report.by_day[day_of(age)];
                    day.lists += 1;
                    day.bytes += size;
                }
                if age <= MIN_AGE {
                    running.report.young += 1;
                    continue;
                }
                found.push(Candidate {
                    list,
                    size,
                    sidecar: sidecar.key,
                });
            }
            if !found.is_empty() && !self.collect(found, &mut running).await? {
                return Ok(());
            }
            match page.next {
                None => {
                    running.report.whole = true;
                    return Ok(());
                }
                Some(next) => after = Some(next),
            }
            tokio::time::sleep(self.config.pause).await;
        }
    }

    /// Asks the pack index and the rows about the old lists of a page, then deletes (or counts)
    /// those neither keeps. Returns `false` when the pass was abandoned part way.
    async fn collect(
        &self,
        found: Vec<Candidate>,
        running: &mut Running<'_>,
    ) -> Result<bool, PassError> {
        let keys: Vec<String> = found
            .iter()
            .filter(|c| c.size.is_some())
            .map(|c| c.list.as_str().to_owned())
            .collect();
        let index = Arc::clone(&self.index);
        let free = offload("pack index", move || index.unreferenced_keys(&keys)).await?;
        let free: BTreeSet<String> = free.map_err(PassError::Index)?.into_iter().collect();
        // Read after the page's ages, just before its deletes: an operation opened before
        // this read is seen.
        let rows = Arc::clone(&self.rows);
        let named = offload("operations' rows", move || rows.named_lists()).await?;
        let named = named.map_err(PassError::Rows)?;
        for candidate in found {
            let report = &mut *running.report;
            if candidate.size.is_some() && !free.contains(candidate.list.as_str()) {
                report.recorded += 1;
                continue;
            }
            if named.contains(candidate.list.as_str()) {
                report.named += 1;
                continue;
            }
            if !self.config.apply {
                if let Some(size) = candidate.size {
                    report.would_delete_lists += 1;
                    report.would_delete_bytes += size;
                }
                report.would_delete_sidecars += 1;
                continue;
            }
            if !(self.is_leader)() {
                report.abandoned = true;
                return Ok(false);
            }
            // The list goes first: a sidecar left by a failed delete names a list that is gone
            // and is deleted by a later pass, while a list left without its sidecar would never
            // be listed again.
            if let Some(size) = candidate.size {
                if let Err(error) = self.stores.lists.delete(&candidate.list).await {
                    running.failed(error)?;
                    continue;
                }
                running.report.deleted_lists += 1;
                running.report.deleted_bytes += size;
            }
            if let Err(error) = self.stores.sidecars.delete(&candidate.sidecar).await {
                running.failed(error)?;
                continue;
            }
            running.failures = 0;
            running.report.deleted_sidecars += 1;
        }
        Ok(true)
    }
}
