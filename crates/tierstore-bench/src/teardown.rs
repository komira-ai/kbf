//! `verify-teardown`: PASS or FAIL per check, each from the before-snapshot preflight took and
//! the state now. Unprivileged, read-only.
//!
//! To fill, as [`crate::result::Check`]s: the test buckets are empty and then gone, with no
//! multipart upload left; the test accounts are gone; the policy and account configuration of the
//! live buckets is unchanged (hash equal); the live binary's digest and the live units'
//! properties (minus the fields that vary with time) are unchanged; the firewall ruleset hash
//! equals the before-snapshot's and the tool's own table is absent. The request log is the
//! proof that no request named a live bucket: the check reads it and fails on any record whose
//! bucket is not a test bucket.

use std::path::Path;

use crate::config::RunConfig;
use crate::error::BenchError;
use crate::result::Check;

/// Runs every check against the snapshot file `before` that preflight wrote. Not built.
///
/// # Errors
/// [`BenchError::NotYet`] today.
pub async fn run(_config: &RunConfig, _before: &Path) -> Result<Vec<Check>, BenchError> {
    Err(BenchError::NotYet("verify-teardown"))
}
