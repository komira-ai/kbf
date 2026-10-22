//! The data side of the action-size estimator's proof: a read-only exporter of the farm's
//! run history, and (in a later change) a memory-only replay of that export through the
//! real scheduler (`docs/design/scheduler.md`, learned sizes).
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

#![forbid(unsafe_code)]

pub mod cli;
pub mod export;
pub mod readonly;
pub mod row;

pub use cli::main;
