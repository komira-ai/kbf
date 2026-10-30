//! The data side of the action-size estimator's proof: a read-only exporter of the farm's
//! run history, and a memory-only replay of that export through the real scheduler
//! (`docs/design/scheduler.md`, learned sizes).
//!
//! # The exporter
//!
//! `kbf-estimator-replay export --api ADDR --token-file FILE --since T --until T --out DIR`
//! walks the operator API's history routes ([`export`]) and writes one JSON line per
//! finished attempt into `DIR/runs-YYYY-MM-DD.jsonl`, one file for each UTC day the
//! attempt was queued on ([`row::Row`]). It is run by whoever has access to a farm, with
//! a read-only credential, and reads nothing but `GET` routes: [`readonly::ReadOnly`]
//! refuses every other method before anything is sent. It writes only under `--out`.
//!
//! What the export cannot say is listed with the walk ([`export`]) and on the row
//! ([`row::Row`]): no CPU architecture, node capacity as it is now, a booking only for
//! killed runs, no measuring instrument (`basis`) until the server reports one.
//!
//! # The replay
//!
//! `kbf-estimator-replay replay --from DIR [--train-days 20 --score-days 10]
//! [--with-upper-bound]` reads an export ([`replay::trace`]), submits its operations to a
//! real [`kbf_sched::Scheduler`] on nodes of the recorded capacity ([`replay::sim`]) and
//! prints, for each arm ([`replay::Arm`]), the own-limit kills, ladder climbs, queue wait
//! and backlog drain of the days after the training days, with the share of operations
//! the estimator resolved at each level, the excluded-sample funnel and the limits
//! ([`replay::report`]). Nothing is written and nothing leaves the process.
//!
//! It is an upper bound on what the phase-1 key floor can gain and a rough check that it
//! does not kill more, on one history, with a model of memory need. Only a concurrent
//! holdout on the farm supports a claim.
//!
//! This crate is a workspace member under the coverage gate (99% of lines and branches):
//! the verbs are library functions over injected I/O, `main` is three lines, and every
//! rule of the replay has a test that asserts it.

#![forbid(unsafe_code)]

pub mod cli;
pub mod export;
pub mod readonly;
pub mod replay;
pub mod row;
pub mod slowdown;

pub use cli::main;
