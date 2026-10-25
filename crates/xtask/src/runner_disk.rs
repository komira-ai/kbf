//! `runner-disk`: the check that the hosted runner's disks have the room the coverage run
//! needs, before it starts.
//!
//! The instrumented workspace build and the tests' temporary files outgrew the root disk of a
//! hosted runner, and the file store refuses to write below 5% free, so about thirty tests
//! failed with "Full" and the log never said the runner was out of disk. The `test` job
//! puts the build on the runner's second disk and runs this first: it names the disk that is
//! short and by how much, or that the second disk is not a separate disk at all.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use clap::Args as ClapArgs;

use crate::Error;

/// A gibibyte, the unit of the flags (and of `df -h`).
const GIB: u64 = 1 << 30;

/// Flags of `runner-disk`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// The second disk's mount point (`/mnt` on a hosted Linux runner).
    #[arg(long)]
    pub disk: PathBuf,
    /// The least free space the second disk may have, in GiB.
    #[arg(long)]
    pub disk_min_free_gb: u64,
    /// The root disk's mount point.
    #[arg(long, default_value = "/")]
    pub root: PathBuf,
    /// The least free space the root disk may have, in GiB.
    #[arg(long)]
    pub root_min_free_gb: u64,
}

/// What is known of one disk: the device it is, and the bytes free to an ordinary user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Disk {
    /// The device number of the directory.
    pub device: u64,
    /// Bytes available to a non-root user.
    pub free_bytes: u64,
}

/// Read `path`'s device and free space.
pub fn probe(path: &Path) -> Result<Disk, Error> {
    let what = || format!("disk {}", path.display());
    let meta = std::fs::metadata(path).map_err(Error::spawn(what()))?;
    let stat = rustix::fs::statvfs(path).map_err(|e| Error::Spawn {
        what: what(),
        why: e.to_string(),
    })?;
    Ok(Disk {
        device: meta.dev(),
        free_bytes: stat.f_bavail.saturating_mul(stat.f_frsize),
    })
}

/// Every reason the two disks are not enough, in the order a reader fixes them: `second` is
/// the disk at `second_path`, which must be a different device from `root`'s and have at
/// least `second_min_gb` GiB free; `root` must have `root_min_gb`.
pub fn problems(
    root: &Disk,
    second: &Disk,
    second_path: &Path,
    root_min_gb: u64,
    second_min_gb: u64,
) -> Vec<String> {
    let mut out = Vec::new();
    if root.device == second.device {
        out.push(format!(
            "runner out of disk: {} is not a separate disk on this runner, so the instrumented \
             build has no second disk to go on",
            second_path.display()
        ));
    }
    if second.free_bytes < second_min_gb * GIB {
        out.push(format!(
            "runner out of disk: {} has {} GiB free and the instrumented build needs {second_min_gb} GiB",
            second_path.display(),
            second.free_bytes / GIB,
        ));
    }
    if root.free_bytes < root_min_gb * GIB {
        out.push(format!(
            "runner out of disk: the root disk has {} GiB free and the restored cache and the other builds need {root_min_gb} GiB",
            root.free_bytes / GIB,
        ));
    }
    out
}

/// Check both disks: one line saying how much each has, or every reason it is short.
pub fn run(args: &Args) -> Result<Vec<String>, Error> {
    let root = probe(&args.root)?;
    let second = probe(&args.disk)?;
    let found = problems(
        &root,
        &second,
        &args.disk,
        args.root_min_free_gb,
        args.disk_min_free_gb,
    );
    if !found.is_empty() {
        return Err(Error::Refused(found));
    }
    Ok(vec![format!(
        "disks: {} GiB free on {} (needs {}), {} GiB free on {} (needs {})",
        second.free_bytes / GIB,
        args.disk.display(),
        args.disk_min_free_gb,
        root.free_bytes / GIB,
        args.root.display(),
        args.root_min_free_gb,
    )])
}
