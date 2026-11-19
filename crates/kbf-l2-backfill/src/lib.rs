//! `kbf-l2-backfill`: the operator tool that gives the tree lists written before dual emission
//! their L2 sidecar, and takes the live-key sample that `tierstore store audit --keys` reads
//! (tierstore's `docs/design/fetch-cache-serve.md`, 3.5 and 10.8; `docs/deploy/l2-backfill.md`
//! is the runbook). It is a binary of its own and never part of `kbf`.
//!
//! ```text
//! kbf-l2-backfill --store-endpoint <url> --store-bucket <bucket>
//!     --store-credentials-file <file> --pack-index <postgres-url>
//!     [--mode dry-run|apply] [--within-days <n>] [--trees-per-second <n>] [--limit-trees <n>]
//!     [--report-json <file>] [--audit-keys-out <file> [--audit-keys <n>] [--audit-seed <n>]]
//! ```
//!
//! The backfill pass takes the trees the pack index holds as unlisted within `--within-days`
//! and, for each, reads the list `cas/l/<id>` and hands it to
//! [`kbf_front::action_tree::Sidecars::backfill`]: the engine reads once each object the list
//! names that has no recorded cells, records them, writes the sidecar create-only and drops the
//! tree's unlisted row. A list the store no longer has is counted `gone` (its row is dropped in
//! an apply), an object the store does not have is counted `lost` (the row is kept: the
//! frontend's miss path republishes the tree), and any other failure is counted `failed` (the
//! row is kept).
//!
//! The sample (`--audit-keys-out`) is a uniform random sample of the keys of the packs the index
//! holds live, one key per line. With `--mode dry-run` it is the whole run; with `--mode apply`
//! it follows the backfill.
//!
//! It does nothing unless run, and a run is a dry run unless `--mode apply` is given. The
//! credentials are a JSON file only its owner can read, never a flag, and the index URL carries
//! no password.
//!
//! Exit status: 0 done (or stopped at `--limit-trees`), 2 refused (the command line or a file it
//! names; nothing was changed), 1 failed (a call failed, or a tree failed). The report is
//! printed, and written with `--report-json`, whichever way the run ends.

pub mod app;
pub mod audit;
pub mod backfill;
pub mod cli;
pub mod credentials;
pub mod error;
pub mod files;
pub mod pace;
pub mod report;
pub mod store;

pub use app::{
    OpenIndex, OpenStore, Seams, main_with, main_with_seams, open_bucket, open_postgres, run,
};
pub use cli::{Args, AuditPlan, Mode, Plan};
pub use error::Error;
pub use report::Report;
