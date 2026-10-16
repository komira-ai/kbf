//! Whether the two halves keep one cache (`kbf_storage::local_cache`): the daemon's
//! input cache and the server's hot store share a disk, so the input cache's directory
//! and index are the node's only local copies of blobs, the hot store keeps no flushed
//! copies beside them, and one budget and one fill count both.
//!
//! Only when the hot store is a directory on the filesystem of `--data-dir`: with no hot
//! store, local storage keeps nothing on this disk but the cold store (the durable store,
//! not a cache), and a hot store on another disk is a copy on another disk. Either way
//! each half then keeps what it keeps without the other, and the start says why.

use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::sync::Arc;

use kbf_server::stores::StoreUri;
use kbf_storage::local_cache::LocalCache;

use crate::cli::Flags;

/// Gives both halves of `flags` the one cache when the hot store shares the data
/// directory's disk; the cache, or `None` (each half as without the other).
pub(crate) fn share(flags: &mut Flags) -> Option<Arc<LocalCache>> {
    let data = flags.daemon.data_dir();
    let Some(StoreUri::File(hot)) = &flags.server.hot_store else {
        tracing::info!("one cache: off (no hot store directory)");
        return None;
    };
    if !same_disk(hot, &data) {
        let (hot, data) = (hot.display(), data.display());
        tracing::info!("one cache: off ({hot} is not on the filesystem of {data})");
        return None;
    }
    let local = Arc::new(LocalCache::new());
    flags.server.local_cache = Some(Arc::clone(&local));
    flags.daemon.share_cache(Arc::clone(&local));
    let (hot, data) = (hot.display(), data.display());
    tracing::info!("one cache: {data}/cache, with the hot store {hot} on its filesystem");
    Some(local)
}

/// Whether `a` and `b` are on one filesystem, each judged by its nearest existing
/// directory (neither need exist yet).
fn same_disk(a: &Path, b: &Path) -> bool {
    matches!((device(a), device(b)), (Some(a), Some(b)) if a == b)
}

/// The device of `path`, or of its nearest ancestor that exists.
fn device(path: &Path) -> Option<u64> {
    path.ancestors()
        .find_map(|at| std::fs::metadata(at).ok())
        .map(|found| found.dev())
}

#[cfg(test)]
#[path = "one_cache_tests.rs"]
mod tests;
