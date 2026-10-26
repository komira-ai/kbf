//! `runner-disk`: the check that the hosted runner's disk has the room the coverage run
//! needs, before it starts.
//!
//! The instrumented workspace build and the tests' temporary files, on top of a cache that
//! restored a second build of the workspace, ran the runner's disk below the 5% free-space
//! floor of the file store, so about thirty tests failed with "Full" and the log never said
//! the runner was out of disk. The `test` job runs this before the tests: it names the disk
//! and how much it has against what the run needs.

use std::path::{Path, PathBuf};

use clap::Args as ClapArgs;

use crate::Error;

/// A gibibyte, the unit of the flag (and of `df -h`).
const GIB: u64 = 1 << 30;

/// Flags of `runner-disk`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// A path on the disk to check (the workspace).
    #[arg(long)]
    pub path: PathBuf,
    /// The least free space that disk may have, in GiB.
    #[arg(long)]
    pub min_free_gb: u64,
}

/// The bytes free to an ordinary user on the disk `path` is on.
pub fn free_bytes(path: &Path) -> Result<u64, Error> {
    let stat = rustix::fs::statvfs(path).map_err(|e| Error::Spawn {
        what: format!("disk {}", path.display()),
        why: e.to_string(),
    })?;
    Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
}

/// Why `free` bytes at `path` are short of `min_gb` GiB: `None` when they are enough.
pub fn problem(path: &Path, free: u64, min_gb: u64) -> Option<String> {
    (free < min_gb.saturating_mul(GIB)).then(|| {
        format!(
            "runner out of disk: the disk of {} has {} GiB free and the tests need {min_gb} GiB \
             (the instrumented build, the tests' temporary files and the file store's 5% floor); \
             something on the runner, or in the restored cache, grew",
            path.display(),
            free / GIB,
        )
    })
}

/// Check the disk: one line saying how much it has, or why it is short.
pub fn run(args: &Args) -> Result<Vec<String>, Error> {
    let free = free_bytes(&args.path)?;
    if let Some(said) = problem(&args.path, free, args.min_free_gb) {
        return Err(Error::Refused(vec![said]));
    }
    Ok(vec![format!(
        "disk of {}: {} GiB free (needs {})",
        args.path.display(),
        free / GIB,
        args.min_free_gb
    )])
}
