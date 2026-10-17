//! What the suite leaves behind: nothing but what the store refuses to delete.
//!
//! After the last check [`sweep`] lists everything under the run's own prefix and deletes
//! it, whether the checks passed or not. It never touches a key outside that prefix.

use std::collections::BTreeSet;

use futures::{StreamExt as _, stream};

use super::LOCK_KEY;
use crate::{KeyPrefix, ListToken, ObjectKey, ObjectStore, ObjectStoreError, PageSize};

/// How many deletes are in flight at once.
const DELETES: usize = 16;
/// Pages of [`PageSize::MAX`] keys the sweep reads before it gives up on a listing that
/// does not end: far more than any run writes.
const MAX_PAGES: usize = 100;

/// What the sweep did to the run's prefix.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cleanup {
    /// Objects deleted.
    pub deleted: usize,
    /// Objects the store refused to delete because they are still under Object Lock
    /// retention. Expected after a run against a store with Object Lock; not a failure.
    pub retained: Vec<ObjectKey>,
    /// Everything else that went wrong (a listing that failed, a delete refused for
    /// another reason): objects may remain under the prefix.
    pub failed: Vec<String>,
}

/// Deletes every object under `prefix`; reports what it could not.
pub(super) async fn sweep<S: ObjectStore>(store: &S, prefix: &KeyPrefix) -> Cleanup {
    let mut cleanup = Cleanup::default();
    let (keys, listing) = list_all(store, prefix).await;
    if let Err(why) = listing {
        cleanup.failed.push(why);
    }
    let lock_key = prefix.key(LOCK_KEY).ok();
    let answers: Vec<_> = stream::iter(&keys)
        .map(|k| async move { (k, store.delete(k).await) })
        .buffer_unordered(DELETES)
        .collect()
        .await;
    for (k, answer) in answers {
        match answer {
            Ok(()) => cleanup.deleted += 1,
            // S3 answers a delete of a retained object with a 403, not a name of its
            // own; the lock check's object is the only one the suite retains.
            Err(ObjectStoreError::Locked(_)) => cleanup.retained.push(k.clone()),
            Err(_) if lock_key.as_ref() == Some(k) => cleanup.retained.push(k.clone()),
            Err(e) => cleanup.failed.push(format!("delete {k}: {e}")),
        }
    }
    cleanup.retained.sort();
    cleanup.failed.sort();
    cleanup
}

/// Every key under `prefix` the listing gives, and why it stopped early if it did.
async fn list_all<S: ObjectStore>(
    store: &S,
    prefix: &KeyPrefix,
) -> (BTreeSet<ObjectKey>, Result<(), String>) {
    let mut keys = BTreeSet::new();
    let mut after: Option<ListToken> = None;
    for _ in 0..MAX_PAGES {
        let page = match store.list(prefix, after.as_ref(), PageSize::MAX).await {
            Ok(page) => page,
            Err(e) => return (keys, Err(format!("list {prefix:?} for cleanup: {e}"))),
        };
        keys.extend(page.objects.into_iter().map(|o| o.key));
        match page.next {
            Some(t) => after = Some(t),
            None => return (keys, Ok(())),
        }
    }
    let why = format!("list {prefix:?} for cleanup: no end after {MAX_PAGES} pages");
    (keys, Err(why))
}
