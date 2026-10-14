//! The local write tier in the server (`docs/design/write-path.md`, section 6): the
//! start that attaches `--write-tier-dir` to the cache, and the background task that
//! seals and flushes its blocks.

#![allow(
    dead_code,
    unreachable_code,
    unused_variables,
    reason = "red-first stand-in: the next commit implements the write tier's flusher, reads and start"
)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use kbf_front::{Cache, MetaLog, TierStartError, TierStartup};
use kbf_objstore::ObjectStore;
use kbf_store::StdFs;

use crate::config::{Args, ConfigError, SERVER};

/// How often the flusher runs a round ([`Cache::flush_tier`]).
pub const FLUSH_EVERY: Duration = Duration::from_secs(1);

/// Why the write tier could not be attached; the server does not start.
#[derive(Debug, thiserror::Error)]
pub enum WriteTierError {
    /// The flags are refused.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The directory could not be created.
    #[error("--write-tier-dir: {0}")]
    Dir(std::io::Error),
    /// An earlier start's files could not be deleted.
    #[error("--write-tier-dir: {0}")]
    Clear(kbf_tier::TierError),
    /// The start was refused (a wrong or missing `TIER` while objects are staged, an
    /// abandon flag naming another tier, a disk or log failure).
    #[error("--write-tier-dir: {0}")]
    Start(#[from] TierStartError),
}

/// `cache` with the tier the flags name attached, and what its start found; the cache
/// as it was when the tier is off.
///
/// The tier runs over `--meta-log=memory` only ([`Args::write_tier`] refuses the Raft
/// log), whose index starts empty, so every file an earlier start left in the
/// directory is deleted first
/// ([`kbf_tier::clear`]): none of it is indexed, and the new log's epochs start from 1
/// again, so an old block's name could be one of this start's objects.
///
/// # Errors
/// [`WriteTierError`].
pub async fn attach<M: MetaLog, O: ObjectStore>(
    cache: Cache<M, O>,
    args: &Args,
) -> Result<(Cache<M, O>, Option<TierStartup>), WriteTierError> {
    return Ok((cache, None));
    let Some((dir, options)) = args.write_tier()? else {
        return Ok((cache, None));
    };
    let fs = StdFs::new(dir).map_err(WriteTierError::Dir)?;
    // `write_tier` refuses every log but the memory one, which starts empty.
    let cleared = kbf_tier::clear(&fs).map_err(WriteTierError::Clear)?;
    tracing::info!(
        files = cleared.len(),
        "write tier directory cleared for a memory index"
    );
    let (cache, startup) = cache
        .attach_tier(fs, options, SERVER, args.tier.abandon())
        .await?;
    tracing::info!(
        tier = %startup.tier,
        registered = startup.registered,
        abandoned = startup.abandoned.len(),
        lost = startup.recovery.lost.len(),
        orphans = startup.recovery.removed.len(),
        unsealed = startup.recovery.unsealed.len(),
        sealed = startup.recovery.sealed.len(),
        "write tier attached"
    );
    Ok((cache, Some(startup)))
}

/// Runs a flusher round every `every`, for as long as the task lives. A round that
/// fails (the metadata log unavailable) is logged and the next one tries again.
pub async fn run_flusher<M: MetaLog, O: ObjectStore>(cache: Arc<Cache<M, O>>, every: Duration) {
    let mut tick = tokio::time::interval(every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if let Err(error) = cache.flush_tier(Instant::now()).await {
            tracing::warn!(%error, "write tier flusher round failed");
        }
    }
}
