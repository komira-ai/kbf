//! The tier's flags.

use std::path::PathBuf;
use std::time::Duration;

use crate::tier::TierOptions;

/// The write tier's flags, for a server's command line (`#[command(flatten)]`). The
/// tier is off unless `--write-tier-dir` is given.
#[derive(Clone, Debug, PartialEq, Eq, clap::Args)]
pub struct TierArgs {
    /// The directory of the local write tier's block files, on NVMe or SSD. Without
    /// it there is no tier: an acknowledged blob is in the object store.
    #[arg(long, requires = "write_tier_bytes")]
    pub write_tier_dir: Option<PathBuf>,

    /// The most bytes the tier's block files may hold before they are flushed (open
    /// uploads' reservations included). At the budget, writes take the direct path.
    #[arg(long, requires = "write_tier_dir", value_parser = clap::value_parser!(u64).range(1..))]
    pub write_tier_bytes: Option<u64>,

    /// How long, in seconds, a grouped block takes new writes before it is sealed for
    /// the flusher.
    #[arg(
        long,
        default_value_t = TierOptions::SEAL_AFTER.as_secs(),
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    pub write_tier_seal_after: u64,
}

impl TierArgs {
    /// The directory and options, or `None` when the tier is off.
    #[must_use]
    pub fn config(&self) -> Option<(PathBuf, TierOptions)> {
        let (dir, budget) = self.write_tier_dir.clone().zip(self.write_tier_bytes)?;
        let options = TierOptions {
            seal_after: Duration::from_secs(self.write_tier_seal_after),
            ..TierOptions::new(budget)
        };
        Some((dir, options))
    }
}
