//! Whether the running proxy is still the installed program: the answer behind
//! `restart_pending` in `GET /v1/update/frontend` (`docs/design/frontend-proxy.md`, section
//! 10). There is no marker file and no maintainer script; the package manager replaces the
//! program file and this module compares file identities.
//!
//! [`RunningBinary::capture`] is called once, when the proxy starts. [`RunningBinary::replaced`]
//! is then a pair of `stat` calls and can be asked on every request.
//!
//! What is compared, per platform, is `(st_dev, st_ino)`:
//! - Linux: the running identity is the metadata of `/proc/self/exe`, which follows to the
//!   inode the kernel still holds for the running image, even after a rename has put another
//!   file at the install path (where `readlink` would say `(deleted)`). The installed
//!   identity is the metadata of `/usr/bin/kbf-installer`.
//! - macOS: the running identity is the metadata of the canonicalized `current_exe()` taken
//!   at start, since the system has no `/proc`; the installed identity is the metadata of
//!   `/usr/local/kbf-installer/bin/kbf-installer`.
//!
//! Decisions:
//! - A file replaced by a rename (what apt, dnf and the macOS installer do) has a new inode:
//!   replaced.
//! - A file rewritten in place (truncate and write) keeps its inode: **not** replaced. That
//!   is the documented limit: the package managers never write in place, and the kernel
//!   refuses to open a running image for writing (`ETXTBSY`), so on Linux the case cannot
//!   arise against the running file.
//! - An install path that cannot be read, including one that is gone (the package was
//!   removed), is replaced: the running program is no longer the installed one. A restart
//!   is the operator's answer in both cases.
//! - A proxy started from a file that is not the install path (a build tree) is replaced
//!   from the first call: it is not the installed program.

use std::fs::Metadata;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::proxy::api::traits::RestartPending;

/// Where the package installs the program.
#[cfg(target_os = "macos")]
pub const INSTALLED: &str = "/usr/local/kbf-installer/bin/kbf-installer";
#[cfg(not(target_os = "macos"))]
pub const INSTALLED: &str = "/usr/bin/kbf-installer";

/// The file the running image came from, as the process can name it now.
#[cfg(target_os = "macos")]
fn running_path() -> io::Result<PathBuf> {
    std::env::current_exe()?.canonicalize()
}
#[cfg(not(target_os = "macos"))]
fn running_path() -> io::Result<PathBuf> {
    Ok(PathBuf::from("/proc/self/exe"))
}

fn identity(m: &Metadata) -> (u64, u64) {
    (m.dev(), m.ino())
}

/// The identity of the program that is running, and the path whose file it is compared to.
#[derive(Debug, Clone)]
pub struct RunningBinary {
    running: (u64, u64),
    installed: PathBuf,
}

impl RunningBinary {
    /// The running program against [`INSTALLED`]. Call once, at proxy start.
    pub fn capture() -> io::Result<RunningBinary> {
        RunningBinary::from_paths(&running_path()?, Path::new(INSTALLED))
    }

    /// The identity of the file at `running` (followed through links), compared later to the
    /// file at `installed`. For tests and for a caller that knows both paths.
    pub fn from_paths(running: &Path, installed: &Path) -> io::Result<RunningBinary> {
        Ok(RunningBinary {
            running: identity(&std::fs::metadata(running)?),
            installed: installed.to_owned(),
        })
    }

    /// Whether the installed path now names a different file than the running program, or
    /// none that can be read. See the module notes for the cases.
    pub fn replaced(&self) -> bool {
        !std::fs::metadata(&self.installed).is_ok_and(|m| identity(&m) == self.running)
    }
}

impl RestartPending for RunningBinary {
    fn restart_pending(&self) -> bool {
        self.replaced()
    }
}

#[cfg(test)]
#[path = "running_binary_tests.rs"]
mod tests;
