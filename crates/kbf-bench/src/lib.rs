//! `kbf-bench`: times blob uploads to a kbf-server's CAS, and reads back, the way an
//! action's outputs are stored; and times no-op actions, Execute to done.
//!
//! An upload or read builds a tree ([`tree`]: generated from a shape and a seed, or read
//! from a directory), connects as a transfer client does ([`connect`]), makes its requests a set
//! number at a time and times each one ([`bench`]), and reports the wall time, the rates
//! and per-request latency percentiles ([`report`]). `noop` ([`noop`]) sends Executes of
//! actions that run [`program`], a program that exits 0, to one or more frontends and
//! records each run as a JSON line. The command line is [`cli`]; the README shows the
//! runs.

pub mod bench;
pub mod cli;
pub mod connect;
pub mod noop;
pub mod program;
pub mod report;
pub mod tree;
