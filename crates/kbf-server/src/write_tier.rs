//! The local write tier in the server (`docs/design/write-path.md`, section 6): the
//! start that attaches `--write-tier-dir` to the cache, over either metadata log, and
//! the background task that seals and flushes its blocks.

use std::sync::Arc;
use std::time::{Duration, Instant};

use kbf_front::{Cache, MemoryMetaLog, MetaLog, MetaLogError, TierStartError, TierStartup};
use kbf_log::MachineVersion;
use kbf_objstore::ObjectStore;
use kbf_store::{Fs, StdFs};

use crate::RaftMetaLog;
use crate::config::{Args, SERVER};

/// How often the flusher runs a round ([`Cache::flush_tier`]).
pub const FLUSH_EVERY: Duration = Duration::from_secs(1);

/// Why the write tier could not be attached; the server does not start.
#[derive(Debug, thiserror::Error)]
pub enum WriteTierError {
    /// The metadata log could not be readied for the tier's commands
    /// ([`TierLog::ready_for_tier`]).
    #[error("--write-tier-dir: {0}")]
    Log(MetaLogError),
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

/// A metadata log the write tier can run over.
pub trait TierLog: MetaLog {
    /// Readies the log, and the tier directory `dir`, for a start of the tier: called
    /// once per start, only when a tier is configured, before the tier attaches.
    ///
    /// # Errors
    /// The log cannot take the tier's commands, or the directory cannot be readied;
    /// the server does not start.
    fn ready_for_tier(&self, dir: &StdFs) -> Result<(), WriteTierError>;
}

impl TierLog for MemoryMetaLog {
    /// Deletes every file an earlier start left in `dir` ([`kbf_tier::clear`]). The
    /// memory log takes every command as it is, but its index starts empty: nothing
    /// in the directory is indexed, and its writer epochs start from 1 again, so an
    /// old block's name could be one of this start's objects.
    fn ready_for_tier(&self, dir: &StdFs) -> Result<(), WriteTierError> {
        let cleared = kbf_tier::clear(dir).map_err(WriteTierError::Clear)?;
        tracing::info!(
            files = cleared.len(),
            "write tier directory cleared for a memory index"
        );
        Ok(())
    }
}

impl<F> TierLog for RaftMetaLog<F>
where
    F: Fs + Send + 'static,
    F::File: Send + 'static,
{
    /// Raises the log to machine version 2, whose format carries the tier's commands
    /// ([`RaftMetaLog::raise_machine_version`]); a log already there commits nothing.
    /// From then on a binary that runs only version 1 refuses the log. A server
    /// without a tier never calls it, so its log stays one an older binary reads.
    /// `dir` is left as it is: the tier's start recovers it against the committed
    /// staged table, which outlives the process.
    fn ready_for_tier(&self, _: &StdFs) -> Result<(), WriteTierError> {
        self.raise_machine_version(MachineVersion::V2)
            .map_err(WriteTierError::Log)
    }
}

/// `cache` with the tier the flags name attached, and what its start found; the cache
/// as it was when the tier is off.
///
/// With a tier, the log and the directory are readied first
/// ([`TierLog::ready_for_tier`]: the Raft log is raised to machine version 2 once; the
/// memory log's directory is cleared), then the tier's start recovers the directory
/// against the committed staged table ([`Cache::attach_tier`]). Without a tier the log
/// is not touched.
///
/// # Errors
/// [`WriteTierError`].
pub async fn attach<M: TierLog, O: ObjectStore>(
    cache: Cache<M, O>,
    args: &Args,
) -> Result<(Cache<M, O>, Option<TierStartup>), WriteTierError> {
    let Some((dir, options)) = args.write_tier() else {
        return Ok((cache, None));
    };
    let fs = StdFs::new(dir).map_err(WriteTierError::Dir)?;
    cache.meta().ready_for_tier(&fs)?;
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
