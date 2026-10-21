//! The size estimator: predicts the CPU and memory an action will use from what
//! earlier runs of similar actions used.
//!
//! This is a pure crate: no async runtime, network, clock, randomness or hashed
//! collections. The layering test in `kbf-integration-test` and the lists in `clippy.toml` enforce it.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]
