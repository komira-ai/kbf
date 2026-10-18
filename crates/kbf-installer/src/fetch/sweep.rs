//! What a crash leaves behind, removed at the start of every run (design 7.3): everything
//! under `work/` (a partial download is never resumed or trusted: its bytes were not
//! verified), and each directory of `releases/` that has no `VERIFIED` marker.
//!
//! A directory without the marker that `current` or `previous` points to is **kept**: it is
//! the release the host is running (or last ran), and removing it would delete the binary
//! under a live service. It is reported instead, as unverified and refused as a rollback
//! target ([`Swept::notes`]); a fetch of the same commit replaces it only with identical
//! bytes (see [`crate::fetch::root`]).

use std::fs;
use std::io;
use std::path::Path;

use crate::CommitId;
use crate::fetch::MARKER;

/// What a sweep did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Swept {
    /// The entries of `work/` removed.
    pub work: Vec<String>,
    /// The unmarked, unreferenced release directories removed.
    pub removed: Vec<CommitId>,
    /// The unmarked directories `current` or `previous` names, left in place.
    pub unverified_kept: Vec<CommitId>,
}

impl Swept {
    /// One line per directory kept, for the status file.
    pub fn notes(&self) -> Vec<String> {
        self.unverified_kept
            .iter()
            .map(|c| format!("{c} is unverified, rollback to it refused"))
            .collect()
    }
}

/// The commit a link in `releases/` points at: the last component of its target (`<commit>`
/// or `releases/<commit>`).
fn target_of(link: &Path) -> Option<CommitId> {
    let target = fs::read_link(link).ok()?;
    CommitId::new(target.file_name()?.to_str()?).ok()
}

/// Whether `dir` holds a marker that is a plain file (a link or a directory is not one).
pub fn marked(dir: &Path) -> bool {
    fs::symlink_metadata(dir.join(MARKER)).is_ok_and(|m| m.is_file())
}

/// Whether `current` or `previous` in `releases` names `commit`.
pub fn referenced(releases: &Path, commit: &CommitId) -> bool {
    ["current", "previous"]
        .iter()
        .any(|link| target_of(&releases.join(link)).as_ref() == Some(commit))
}

fn remove(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Sweep `releases` and `work`. A directory that is not there is nothing to sweep.
pub fn sweep(releases: &Path, work: &Path) -> io::Result<Swept> {
    let mut swept = Swept::default();
    if let Some(entries) = read_dir(work)? {
        for entry in entries {
            let entry = entry?;
            remove(&entry.path())?;
            swept.work.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    if let Some(entries) = read_dir(releases)? {
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let commit = entry
                .file_name()
                .to_str()
                .and_then(|n| CommitId::new(n).ok());
            // Only a real directory named by a commit; `current`, `previous` and strangers stay.
            let (Some(commit), true) = (commit, entry.file_type()?.is_dir()) else {
                continue;
            };
            if marked(&path) {
                continue;
            }
            if referenced(releases, &commit) {
                swept.unverified_kept.push(commit);
            } else {
                fs::remove_dir_all(&path)?;
                swept.removed.push(commit);
            }
        }
    }
    swept.work.sort();
    swept.removed.sort();
    swept.unverified_kept.sort();
    Ok(swept)
}

fn read_dir(dir: &Path) -> io::Result<Option<fs::ReadDir>> {
    match fs::read_dir(dir) {
        Ok(entries) => Ok(Some(entries)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
