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
//! # The slowdown report
//!
//! `kbf-estimator-replay slowdown --from DIR [--quiet-pressure-centi 500] [--max-overlap 0]
//! [--baseline-percentile 10] [--json]` reads an export and says, for each successful run,
//! how much slower it took than the same estimator key on a quiet host: `wall / baseline`,
//! with a per-key and an overall median, p90 and share of runs above 1.5x. The text form
//! prints the definition, the funnel and one line per key; `--json` adds every run. The exact
//! definition (what is measured, the key, which approximates the estimator's with an OS-only
//! platform class, the quiet rule, the baseline statistic, how a key
//! with no quiet run is reported) is in the [`slowdown`] module documentation, and the report
//! prints what it cannot see ([`slowdown::LIMITS`]). The quiet signal is the node's CPU
//! pressure at the lease start plus the overlap of exported attempts on the node; the record
//! carries no concurrent-lease count, and a run with no pressure reading is counted apart and
//! never imputed.
//!
//! # Per-build numbers
//!
//! A row carries the invocation (`tool_invocation_id`, one buck2 command) and, when the
//! client sent one, the CI job (`correlated_invocations_id`) of the Execute request its
//! attempt belonged to, as the server's run record holds them ([`row::Row`]). [`builds`]
//! turns rows into per-build numbers (the server's action span, the queue time total, the
//! attempt queue wait distribution) and into per-run values shaped for `js-analysis`'s
//! `RunRecord`. The action span excludes the client's own analysis and local actions: the
//! paper's build wall comes from the client. A row from a server that predates the ids has
//! none, and no build is made of it.
//!
//! # The two-arm comparison
//!
//! `kbf-estimator-replay compare --arm-a DIR --arm-b DIR [--json]` reads two exports and
//! prints, for each arm and as the difference, queue wait and wall percentiles, the makespan
//! and kills per 1,000 attempts split by kind. The definitions (nearest rank, which attempts
//! count, absent against zero) are in the [`compare`] module documentation, and the report
//! prints what the export cannot say ([`compare::NOT_RECORDED`]). It reads and prints only.
//!
//! This crate is a workspace member under the coverage gate (99% of lines and branches):
//! the verbs are library functions over injected I/O, `main` is three lines, and every
//! rule of the replay has a test that asserts it.

#![forbid(unsafe_code)]

pub mod builds;
pub mod cli;
pub mod compare;
pub mod export;
pub mod readonly;
pub mod replay;
pub mod row;
pub mod slowdown;

pub use cli::main;
