//! The command line.

use std::path::PathBuf;

use clap::Parser;

const EXIT_STATUS: &str = "Exit status: 0 done, 2 refused (nothing was changed), 1 failed.";

/// Copies a frontend's SQLite state file into an empty Postgres store.
///
/// Without --apply this is a dry run: it opens the file read-only, reports each key family
/// with its row count and a SHA-256 digest of its rows, checks the target, and writes
/// nothing. With --apply it copies everything in one transaction, reads it back inside the
/// transaction and again on a new connection after the commit, and compares counts and
/// digests per family; a mismatch before the commit rolls the copy back.
#[derive(Debug, Parser)]
#[command(name = "kbf-state-copy", version, after_help = EXIT_STATUS)]
pub struct Args {
    /// The frontend's SQLite state file. The frontend must be stopped.
    pub state_db: PathBuf,
    /// The empty Postgres store to copy into, as a postgres:// URL.
    pub postgres_url: String,
    /// Copy for real. Without it nothing is written.
    #[arg(long)]
    pub apply: bool,
}
