//! Worker capabilities: parsers over the text a node's OS publishes about its CPU and
//! GPUs, the ISA levels that text implies, and matching of an action's capability
//! request against a node.
//!
//! The code now lives in the `js-caps` crate of the `komira-ai/jobscheduler`
//! repository (`crates/js-caps`), at the revision pinned in the workspace
//! `Cargo.toml`; this crate re-exports it under its old name so that every path
//! (`kbf_caps::NodeCaps`, `kbf_caps::Request`, ...) stays as it was. It is still a pure
//! crate (no async runtime, network, clock, randomness or hashed collections): the
//! layering test in `kbf-integration-test` and the lists in `clippy.toml` enforce it,
//! and `js-caps` depends only on `thiserror`.
//!
//! `tests/fixtures` stays here because `kbf-daemon` reads some of its files with
//! `include_str!`. `tests/wire_keys.rs` pins the platform property names a kbf client
//! and daemon exchange, so a revision that renames one fails here.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub use js_caps::*;
