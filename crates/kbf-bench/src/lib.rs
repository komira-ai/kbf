//! `kbf-bench`: times blob uploads to a kbf-server's CAS, and reads back, the way a
//! daemon stores an action's outputs.
//!
//! A run builds a tree ([`tree`]: generated from a shape and a seed, or read from a
//! directory), connects as the daemon does ([`connect`]), makes its requests a set
//! number at a time and times each one ([`bench`]), and reports the wall time, the rates
//! and per-request latency percentiles ([`report`]). The command line is [`cli`]; the
//! README shows a run against a pilot node.

pub mod bench;
pub mod cli;
pub mod connect;
pub mod report;
pub mod tree;
