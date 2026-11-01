//! The deterministic simulation kernel: a virtual clock, a seeded random stream, a
//! message bus between named nodes with delay, drop, duplicate, reorder and partition
//! faults, a run loop that drives [`kbf_types::StateMachine`] nodes, and a trace whose
//! hash identifies the run.
//!
//! The code now lives in the `js-sim` crate of the `komira-ai/jobscheduler` repository
//! (`crates/js-sim`), at the revision pinned in the workspace `Cargo.toml`; this crate
//! re-exports it under its old name so that every path (`kbf_sim::Sim`,
//! `kbf_sim::text`, `kbf_sim::golden`, ...) stays as it was. Its nodes are
//! [`kbf_types::StateMachine`]s because `kbf_types::sched` is `js_types::sched`.
//!
//! A run is a function of its seed and its scenario, and its [`TraceHash`] is the same in
//! any process on any platform. `tests/determinism.rs` and `tests/toy_counter.rs` keep the
//! hashes of their pinned runs in `tests/golden/trace-hashes.txt`, so a revision that
//! changes a trace line fails here.
//!
//! It is still a pure crate (no clock, sleep, network or hashed collections): the layering
//! test in `kbf-integration-test` and the lists in `clippy.toml` enforce it.
#![deny(clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub use js_sim::*;
