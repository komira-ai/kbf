//! `preflight`: read-only checks before anything is written, and the before-snapshot
//! [`crate::teardown`] compares against.
//!
//! To fill: the version of the binary under test and its SHA-256, the flags the live instance
//! runs with (flag parity), the free space and loop-device headroom a test Silo needs, the cgroup
//! `io.max` file, the cold store's version (one read-only query), the hardware facts by role
//! ([`crate::result::Hardware`]), and a [`Snapshot`] of the live buckets' policy and account
//! configuration, the live units' properties and the firewall ruleset (hashes only). Nothing it
//! does writes to a store or changes a machine.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::RunConfig;
use crate::error::BenchError;
use crate::result::Check;

/// What preflight recorded, as name and value (hashes and counts, never contents).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// The facts, by name.
    pub facts: BTreeMap<String, String>,
}

/// The outcome of a preflight.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreflightReport {
    /// One entry per check.
    pub checks: Vec<Check>,
    /// The before-snapshot, written to the result directory for `verify-teardown`.
    pub snapshot: Snapshot,
}

/// Runs the checks. Not built.
///
/// # Errors
/// [`BenchError::NotYet`] today.
pub async fn run(_config: &RunConfig) -> Result<PreflightReport, BenchError> {
    Err(BenchError::NotYet("preflight"))
}
