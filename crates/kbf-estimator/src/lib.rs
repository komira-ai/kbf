//! The size estimator: predicts the CPU and memory an action will use from what
//! earlier runs of similar actions used.
//!
//! The code now lives in the `js-sizing` crate of the `komira-ai/jobscheduler`
//! repository (`crates/js-sizing`), at the revision pinned in the workspace
//! `Cargo.toml`; this crate re-exports it under its old name so that every path
//! (`kbf_estimator::Summaries`, `kbf_estimator::Run`, ...) stays as it was. It is still a
//! pure crate (no async runtime, network, clock, randomness or hashed collections): the
//! layering test in `kbf-integration-test` and the lists in `clippy.toml` enforce it,
//! and `js-sizing` has no dependencies of its own.
//!
//! `tests/codec_golden.rs` keeps the stored form of a summary byte for byte, so a
//! revision that changes it fails here.
#![deny(clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub use js_sizing::*;
