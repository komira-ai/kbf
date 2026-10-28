//! The scheduler core: operation states, leases and fencing, result acceptance, and
//! assignment of actions to workers, as a pure state machine.
//!
//! The code lives in the `js-sched` crate of the `komira-ai/jobscheduler` repository
//! (`crates/js-sched`), at the revision pinned in the workspace `Cargo.toml`, together
//! with its unit tests, its simulation tests and their golden files (one copy, there);
//! this crate re-exports it under its old name so that every path
//! (`kbf_sched::Scheduler`, `kbf_sched::fence::LEASE_GRACE`, ...) stays as it was. It is
//! still a pure crate (no async runtime, network, clock, randomness or hashed
//! collections): the layering test in `kbf-integration-test` and the lists in
//! `clippy.toml` enforce it.
//!
//! What stays in kbf is the bridge between the build's types and the scheduler's: the
//! `ActionKey` and `WorkKey` conversions, and the `Digest` and `ResultRef` ones, are in
//! `kbf-types`.
#![deny(clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub use js_sched::*;
