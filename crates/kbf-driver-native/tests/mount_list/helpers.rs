//! `/sbin/mount` output as mount points, and what a lease did to the volumes outside it.

use std::path::{Path, PathBuf};

/// The mount points in a `/sbin/mount` listing, one line each:
/// `/dev/disk3s1 on /Volumes/Data (apfs, local, journaled)`. The device ends at the
/// first ` on ` and the options start at the last ` (`, so a path holding either stays
/// whole. A line of another shape is skipped.
pub fn mount_points(listing: &str) -> Vec<PathBuf> {
    listing
        .lines()
        .filter_map(|line| {
            let (_, rest) = line.split_once(" on ")?;
            let (point, _) = rest.rsplit_once(" (")?;
            Some(PathBuf::from(point))
        })
        .collect()
}

/// The mount points in `points` at or below `dir`, compared by path component
/// (`lease-1` does not hold `lease-10`'s).
pub fn below(points: &[PathBuf], dir: &Path) -> Vec<PathBuf> {
    points
        .iter()
        .filter(|point| point.starts_with(dir))
        .cloned()
        .collect()
}

/// The mount points of `before` outside `dir` that `after` no longer lists: volumes a
/// lease's removal took that were not the lease's.
pub fn lost_outside(before: &[PathBuf], after: &[PathBuf], dir: &Path) -> Vec<PathBuf> {
    before
        .iter()
        .filter(|point| !point.starts_with(dir) && !after.contains(point))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(list: &[&str]) -> Vec<PathBuf> {
        list.iter().map(PathBuf::from).collect()
    }

    const LISTING: &str = "\
/dev/disk3s1s1 on / (apfs, sealed, local, read-only, journaled)
devfs on /dev (devfs, local, nobrowse)
/dev/disk3s6 on /System/Volumes/VM (apfs, local, noexec, journaled, nobrowse)
/dev/disk5s1 on /private/var/run/com.apple.security.cryptexd/mnt/com.apple.MobileAsset.MetalToolchain-v17.6.42.x (apfs, local, nodev, nosuid, read-only, journaled, nobrowse)
/dev/disk9s1 on /scratch/lease-1/home/my disk (copy) (hfs, local, nodev, nosuid, mounted by adi)
/dev/disk4 on /Volumes/Mac on Disk (hfs, local)
map auto_home on /System/Volumes/Data/home (autofs, automounted, nobrowse)
not a mount line
";

    /// Catches a reading that stops a path at a space or at the first ` (`, ends the
    /// device at the last ` on ` (`rsplit_once`) instead of the first, takes the
    /// device for the mount point, or keeps a line that is not a mount.
    #[test]
    fn a_listing_is_read_as_its_mount_points() {
        assert_eq!(
            mount_points(LISTING),
            paths(&[
                "/",
                "/dev",
                "/System/Volumes/VM",
                "/private/var/run/com.apple.security.cryptexd/mnt/com.apple.MobileAsset.MetalToolchain-v17.6.42.x",
                "/scratch/lease-1/home/my disk (copy)",
                "/Volumes/Mac on Disk",
                "/System/Volumes/Data/home",
            ])
        );
    }

    /// Catches a filter that takes a neighbour sharing a name prefix, or misses the
    /// directory itself or one mounted below it.
    #[test]
    fn the_mounts_below_a_lease_are_those_by_component() {
        let points = paths(&[
            "/scratch/lease-10/m",
            "/scratch/lease-1",
            "/scratch/lease-1/home/m",
            "/",
        ]);
        assert_eq!(
            below(&points, Path::new("/scratch/lease-1")),
            paths(&["/scratch/lease-1", "/scratch/lease-1/home/m"])
        );
    }

    /// Catches a comparison that reports the lease's own volume as lost (it is meant to
    /// go), misses a system volume that went, or reports one that arrived.
    #[test]
    fn only_a_volume_outside_the_lease_counts_as_lost() {
        let lease = Path::new("/scratch/lease-1");
        let before = paths(&["/", "/cryptex/a", "/cryptex/b", "/scratch/lease-1/home/m"]);
        let after = paths(&["/", "/cryptex/b", "/new"]);
        assert_eq!(lost_outside(&before, &after, lease), paths(&["/cryptex/a"]));
        assert_eq!(lost_outside(&before, &before, lease), Vec::<PathBuf>::new());
        // A neighbour's volume lost with the lease's is lost.
        let before = paths(&["/scratch/lease-10/m", "/scratch/lease-1/m"]);
        assert_eq!(
            lost_outside(&before, &[], lease),
            paths(&["/scratch/lease-10/m"])
        );
    }
}
