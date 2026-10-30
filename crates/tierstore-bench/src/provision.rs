//! `provision`: the test buckets, one scoped account per store whose policy reaches only those
//! buckets, and a 30 GiB quota on each bucket.
//!
//! Run by engineering, never by the harness's own credentials: the admin key pair is read from
//! standard input, used for this verb only and never stored. The default is a dry run that
//! prints what it would create; `--apply` creates it. The accounts it makes are the ones in the
//! run file, and the bucket policy is the isolation proof: the account cannot reach any other
//! bucket. The matching removal is in [`crate::teardown`]'s check list.

use crate::config::RunConfig;
use crate::error::BenchError;

/// What a provision run does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Print the plan; change nothing.
    DryRun,
    /// Create the buckets, the accounts and the quotas.
    Apply,
}

/// One line per step taken or planned.
pub type Steps = Vec<String>;

/// Creates (or, in a dry run, lists) the test buckets, scoped accounts and quotas. Not built.
///
/// # Errors
/// [`BenchError::NotYet`] today.
pub async fn run(_config: &RunConfig, _mode: Mode) -> Result<Steps, BenchError> {
    Err(BenchError::NotYet("provision"))
}
