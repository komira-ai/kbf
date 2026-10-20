//! Disk sizing for the two farm binaries: what a directory's filesystem holds and how
//! much of it a cache, a tree store or a write tier may take.
//!
//! Nothing here is a flag's default to hard-code per machine. A directory's budget is a
//! share of its filesystem's size and free space, after a floor the filesystem keeps
//! free for everything else on it (build scratch, the metadata log, the operating
//! system): [`keep_free`]. The caller may only lower it (`--data-max-bytes`,
//! `--hot-store-max-bytes`).
//!
//! Two ways to take a share, for two kinds of directory:
//!
//! - [`Space::of_capacity`]: a share of the filesystem's size less the floor. For a
//!   directory that fills up on purpose and evicts to keep the floor free (the daemon's
//!   input cache): a share of the *free* space would shrink at every restart, by what
//!   the cache already holds. The server's `file://` cold store evicts to the same
//!   [`keep_free`] floor, read afresh at each write, with no budget of its own.
//! - [`Space::of_free`]: a share of the free space less the floor (at most half of what
//!   is free). For a directory that never evicts to a free-space floor (the server's
//!   write tier, which only stops taking writes at its budget). The directory's own
//!   bytes count as free ([`Space::with_held`]): they are the budget's to reuse, and
//!   without them a restart would shrink the budget by what the tier holds.
//!
//! [`FreeSpace`] is where the numbers come from, so a test can give a fake filesystem
//! any size; [`Statvfs`] is the real one.

use std::io;
use std::path::Path;

/// 1 GiB.
const GIB: u64 = 1 << 30;

/// The filesystem's size and free space, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Space {
    /// The filesystem's size.
    pub total: u64,
    /// The bytes a process that is not root may still write (`f_bavail`).
    pub available: u64,
}

/// What a filesystem keeps free for everything else on it: 5% of its size, and at
/// least 1 GiB (or the whole filesystem, when it is smaller).
#[must_use]
pub fn keep_free(total: u64) -> u64 {
    (total / 20).max(GIB).min(total)
}

impl Space {
    /// `percent` of the filesystem's size less [`keep_free`], at most `cap`. 0 when the
    /// filesystem is no larger than the floor.
    #[must_use]
    pub fn of_capacity(&self, percent: u64, cap: Option<u64>) -> u64 {
        let room = self.total.saturating_sub(keep_free(self.total));
        cap_at(percent_of(room, percent), cap)
    }

    /// This space with `held` more bytes free: what a directory already holds is
    /// available to that directory. Never more than the filesystem's size.
    #[must_use]
    pub fn with_held(&self, held: u64) -> Self {
        Self {
            total: self.total,
            available: self.available.saturating_add(held).min(self.total),
        }
    }

    /// `percent` of the free space less the floor, at most `cap`. The floor is
    /// [`keep_free`], but never more than half the free space: a nearly full disk gets a
    /// small budget, not none, and only a disk with nothing free gets 0.
    #[must_use]
    pub fn of_free(&self, percent: u64, cap: Option<u64>) -> u64 {
        let floor = keep_free(self.total).min(self.available / 2);
        let room = self.available.saturating_sub(floor);
        cap_at(percent_of(room, percent), cap)
    }
}

/// `percent` percent of `bytes` (rounded down, no overflow; at most all of them).
#[must_use]
pub fn percent_of(bytes: u64, percent: u64) -> u64 {
    u64::try_from(u128::from(bytes) * u128::from(percent.min(100)) / 100).unwrap_or(u64::MAX)
}

fn cap_at(bytes: u64, cap: Option<u64>) -> u64 {
    cap.map_or(bytes, |cap| bytes.min(cap))
}

/// Where a filesystem's size and free space come from.
pub trait FreeSpace {
    /// The filesystem `path` is on, or that the nearest existing ancestor of `path` is
    /// on, when `path` is not made yet.
    ///
    /// # Errors
    /// No ancestor of `path` can be read.
    fn space(&self, path: &Path) -> io::Result<Space>;

    /// The bytes of the regular files under `dir`, at any depth; 0 when `dir` is not
    /// made yet. A source that models only sizes (a test's) holds nothing.
    ///
    /// # Errors
    /// A directory under `dir` cannot be read.
    fn held(&self, _dir: &Path) -> io::Result<u64> {
        Ok(0)
    }
}

/// The real filesystems: `statvfs`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Statvfs;

impl FreeSpace for Statvfs {
    fn space(&self, path: &Path) -> io::Result<Space> {
        let mut last = io::Error::from(io::ErrorKind::NotFound);
        for dir in path.ancestors() {
            match rustix::fs::statvfs(dir) {
                Ok(stat) => {
                    return Ok(Space {
                        total: stat.f_blocks.saturating_mul(stat.f_frsize),
                        available: stat.f_bavail.saturating_mul(stat.f_frsize),
                    });
                }
                Err(e) => last = e.into(),
            }
        }
        Err(last)
    }

    fn held(&self, dir: &Path) -> io::Result<u64> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };
        let mut bytes = 0_u64;
        for entry in entries {
            let entry = entry?;
            let meta = std::fs::symlink_metadata(entry.path())?;
            bytes = bytes.saturating_add(if meta.is_dir() {
                self.held(&entry.path())?
            } else if meta.is_file() {
                meta.len()
            } else {
                0
            });
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use kbf_testkit::dir::TestDir;

    use super::{FreeSpace, GIB, Space, Statvfs, keep_free, percent_of};

    /// Catches a floor that is not 5% with a 1 GiB minimum, one larger than the
    /// filesystem (a budget that underflows), and the minimum applied to a filesystem
    /// smaller than it.
    #[test]
    fn the_floor_is_five_percent_and_at_least_a_gib() {
        assert_eq!(keep_free(1000 * GIB), 50 * GIB);
        assert_eq!(keep_free(20 * GIB), GIB);
        assert_eq!(keep_free(10 * GIB), GIB);
        assert_eq!(keep_free(GIB / 2), GIB / 2);
        assert_eq!(keep_free(0), 0);
    }

    /// Catches the two shares swapped: a share of capacity ignores what is used (so it
    /// does not shrink at a restart), a share of free space does not; both are after
    /// the floor, both are capped by the caller's maximum, and neither goes below 0 or
    /// overflows.
    #[test]
    fn a_share_of_capacity_and_a_share_of_free_space() {
        let space = Space {
            total: 1000 * GIB,
            available: 300 * GIB,
        };
        assert_eq!(space.of_capacity(50, None), 475 * GIB);
        assert_eq!(space.of_free(50, None), 125 * GIB);
        assert_eq!(space.of_capacity(50, Some(10 * GIB)), 10 * GIB);
        assert_eq!(space.of_free(80, Some(1000 * GIB)), 200 * GIB);
        let full = Space {
            total: 1000 * GIB,
            available: 10 * GIB,
        };
        // The 50 GiB floor is over half of what is free: half is kept.
        assert_eq!(full.of_free(80, None), 4 * GIB);
        let none = Space {
            total: 1000 * GIB,
            available: 0,
        };
        assert_eq!(none.of_free(80, None), 0, "nothing free");
        assert!(full.of_capacity(80, None) > 0);
        assert_eq!(percent_of(u64::MAX, 100), u64::MAX);
        assert_eq!(percent_of(u64::MAX, 50), u64::MAX / 2);
        assert_eq!(percent_of(10, 250), 10);
    }

    /// Catches the real source returning nothing for a directory not made yet (a first
    /// start has no hot store directory): it answers for the nearest ancestor that
    /// exists, and refuses only when none does.
    #[test]
    fn the_real_source_answers_for_a_directory_not_made_yet() {
        let here = std::env::current_dir().expect("a directory");
        let made = Statvfs.space(&here).expect("this directory");
        assert!(made.total > 0);
        assert!(made.available <= made.total);
        let later = Statvfs
            .space(&here.join("not/made/yet"))
            .expect("its ancestor's");
        assert_eq!(later.total, made.total);
        assert!(Statvfs.space(Path::new("relative/nowhere")).is_err());
    }

    /// Catches a directory's own bytes not counted free to it (a restart would shrink
    /// its budget by what it holds), and the sum going past the filesystem's size.
    #[test]
    fn what_a_directory_holds_is_free_to_it() {
        let space = Space {
            total: 1000 * GIB,
            available: 300 * GIB,
        };
        assert_eq!(space.with_held(0), space);
        assert_eq!(space.with_held(100 * GIB).available, 400 * GIB);
        assert_eq!(space.with_held(u64::MAX).available, 1000 * GIB);
    }

    /// Catches a walk that misses nested files, counts a directory or a symlink, or
    /// refuses a directory not made yet.
    #[test]
    fn the_real_source_sums_the_files_under_a_directory() {
        let base = TestDir::under(std::env::temp_dir(), "kbf-threads-held");
        // A directory not made yet: the test's own directory holds it, and is removed.
        let root = base.join("root");
        assert_eq!(Statvfs.held(&root).expect("not made yet"), 0);
        std::fs::create_dir_all(root.join("a/b")).expect("dirs");
        std::fs::write(root.join("top"), [0_u8; 10]).expect("file");
        std::fs::write(root.join("a/mid"), [0_u8; 100]).expect("file");
        std::fs::write(root.join("a/b/deep"), [0_u8; 1000]).expect("file");
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("a/b/deep"), root.join("link")).expect("link");
        assert_eq!(Statvfs.held(&root).expect("held"), 1110);
        assert_eq!(Statvfs.held(&root.join("a")).expect("held"), 1100);
        assert!(
            Statvfs.held(&root.join("top")).is_err(),
            "a file is no directory"
        );
    }
}
