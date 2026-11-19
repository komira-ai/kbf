//! The backfill pass: the unlisted trees of the pack index, one at a time.

use std::sync::Arc;
use std::time::Duration;

use kbf_front::action_tree::{Sidecars, Sleeper};
use kbf_front::large_backfill::LargeStore;
use kbf_objstore::{ObjectKey, ObjectStoreError};
use kbf_pack_index::PackIndex;
use kbf_pack_index::cells::MAX_PAGE;

use crate::cli::Mode;
use crate::error::Error;
use crate::pace::{Clock, Pace};
use crate::report::Backfill;

/// What the pass needs besides the index and the store.
pub struct Settings {
    /// What the run does.
    pub mode: Mode,
    /// How far back the trees are taken from.
    pub within: Duration,
    /// Spaces the trees.
    pub pace: Pace,
    /// The most trees to handle.
    pub limit: Option<u64>,
}

/// The lowercase hex of `bytes`.
#[must_use]
pub fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// How one tree ended.
enum Outcome {
    Written { objects: u64, bytes: u64 },
    WouldWrite,
    Gone,
    Lost(String),
    Failed(String),
}

/// The pass over the trees `settings` selects. `report` is filled as it goes, so a failure
/// leaves the counts of the trees done.
///
/// # Errors
/// [`Error::Failed`] when the index cannot list the unlisted trees.
pub async fn run(
    settings: Settings,
    index: &Arc<dyn PackIndex>,
    store: &Arc<dyn LargeStore>,
    clock: &dyn Clock,
    sleeper: &dyn Sleeper,
    report: &mut Backfill,
) -> Result<(), Error> {
    let Settings {
        mode,
        within,
        mut pace,
        limit,
    } = settings;
    let sidecars = Sidecars::new(Arc::clone(index), Arc::clone(store));
    let mut after: Option<[u8; 32]> = None;
    loop {
        let wanted = limit.map_or(MAX_PAGE as u64, |limit| {
            limit.saturating_sub(report.trees_seen)
        });
        if wanted == 0 {
            return Ok(());
        }
        let page_len = usize::try_from(wanted).map_or(MAX_PAGE, |wanted| wanted.min(MAX_PAGE));
        let page = index
            .unlisted(within, after.as_ref(), page_len)
            .map_err(|error| Error::failed(format!("listing the unlisted trees: {error}")))?;
        let Some(last) = page.last() else {
            report.finished = true;
            return Ok(());
        };
        after = Some(last.tree);
        for unlisted in &page {
            pace.wait(clock, sleeper).await;
            let outcome = handle(mode, index, store, &sidecars, &unlisted.tree).await;
            report.trees_seen += 1;
            note(report, &unlisted.tree, outcome);
        }
    }
}

/// Counts what happened to `tree`.
fn note(report: &mut Backfill, tree: &[u8; 32], outcome: Outcome) {
    match outcome {
        Outcome::Written { objects, bytes } => {
            report.written += 1;
            report.objects_read += objects;
            report.bytes_read += bytes;
        }
        Outcome::WouldWrite => report.would_write += 1,
        Outcome::Gone => report.gone += 1,
        Outcome::Lost(detail) => {
            report.lost += 1;
            report.problem(&hex(tree), "lost", detail);
        }
        Outcome::Failed(detail) => {
            report.failed += 1;
            report.problem(&hex(tree), "failed", detail);
        }
    }
}

/// One tree: its list is read, and the engine backfills it.
async fn handle(
    mode: Mode,
    index: &Arc<dyn PackIndex>,
    store: &Arc<dyn LargeStore>,
    sidecars: &Sidecars,
    tree: &[u8; 32],
) -> Outcome {
    let key = match ObjectKey::new(format!("cas/l/{}", hex(tree))) {
        Ok(key) => key,
        Err(error) => return Outcome::Failed(format!("the list's key: {error}")),
    };
    let len = match store.len(&key).await {
        Ok(Some(len)) => len,
        Ok(None) => return gone(mode, index, tree),
        Err(error) => return Outcome::Failed(format!("the length of {key}: {error}")),
    };
    if mode == Mode::DryRun {
        return Outcome::WouldWrite;
    }
    let list = match read_list(store, &key, len).await {
        Ok(list) => list,
        Err(ObjectStoreError::NotFound(_)) => return gone(mode, index, tree),
        Err(error) => return Outcome::Failed(format!("reading {key}: {error}")),
    };
    match sidecars.backfill(&list).await {
        Ok(done) => Outcome::Written {
            objects: done.objects_read as u64,
            bytes: done.bytes_read,
        },
        Err(kbf_front::action_tree::SidecarError::Lost { key }) => {
            Outcome::Lost(format!("the store has no object {key}"))
        }
        Err(error) => Outcome::Failed(error.to_string()),
    }
}

/// The list is not in the store: in an apply its unlisted row is dropped, as there is nothing
/// to write a sidecar of; a reader that needs the tree republishes it.
fn gone(mode: Mode, index: &Arc<dyn PackIndex>, tree: &[u8; 32]) -> Outcome {
    if mode == Mode::Apply
        && let Err(error) = index.drop_unlisted(tree)
    {
        return Outcome::Failed(format!("dropping the unlisted row: {error}"));
    }
    Outcome::Gone
}

/// The `len` bytes of the list `key`.
async fn read_list(
    store: &Arc<dyn LargeStore>,
    key: &ObjectKey,
    len: u64,
) -> Result<Vec<u8>, ObjectStoreError> {
    if len == 0 {
        return Ok(Vec::new());
    }
    let bytes = store.read_range(key, 0, len).await?;
    Ok(bytes.to_vec())
}
