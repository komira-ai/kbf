//! `kbf-pack-adopt`: the operator tool that puts the segments storage members already flushed
//! to the cold store into the pack index, in place, so that a read finds their blobs through
//! the index (`kbf_pack::Adopter` does the work; `docs/deploy/pack-adopt.md` is the runbook).
//! It is a binary of its own and never part of `kbf`.
//!
//! ```text
//! kbf-pack-adopt --source-store-endpoint <url> --source-store-bucket <bucket>
//!     --source-store-credentials-file <file> --pack-index <postgres-url>
//!     [--mode dry-run|apply] [--pack-store-bucket <bucket>] [--checkpoint-file <file>]
//!     [--report-json <file>] [--segments-per-second <n>] [--footer-concurrency <n>]
//!     [--limit-segments <n>] [--verify-sample <n>] [--fail-on-conflict]
//! ```
//!
//! It does nothing unless run, and a run is a dry run unless `--mode apply` is given. It
//! copies no byte and writes nothing to the object store: the only thing it writes to the
//! object store or the database is the pack index, and only with `--mode apply`; it also
//! writes the checkpoint and report files it is told to. `--mode apply` is refused unless
//! `--pack-store-bucket`, the bucket the frontends write their packs to, is the bucket of the
//! segments; a `file://` source is refused. The credentials are a JSON file only its owner can
//! read, never a flag, and the index URL carries no password.
//!
//! Exit status: 0 done (or stopped at `--limit-segments`), 2 refused (the command line, a file
//! it names, or a checkpoint of another run; nothing was changed), 1 failed (a call failed; the
//! checkpoint says how far the run got). The report is printed, and written with
//! `--report-json`, whichever way the run ends.

pub mod app;
pub mod cli;
pub mod credentials;
pub mod error;
pub mod summary;

pub use app::{OpenIndex, Seams, main_with, main_with_seams, open_postgres, run};
pub use cli::{Args, DRY_RUN_SAMPLE, Mode, Plan};
pub use error::Error;
