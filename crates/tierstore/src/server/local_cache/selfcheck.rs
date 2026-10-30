//! The start-up check of the cache directory: the questions the cache's layout stands on.
//!
//! A hard link needs the file system to do them, and a cache on one that does not would serve
//! wrong bytes or fail every fill. The check makes a probe object under `<path>/.stage/` (which is
//! emptied at every start), gives it a second name under `<path>/obj/` (the link a fill makes, so
//! that a cache whose two directories are on different file systems is refused), and asks four
//! questions of the file system:
//!
//! - `create-only`: a second link to a name that exists is refused and leaves the first alone;
//! - `atomic-visibility`: a name linked to a file that was written whole shows all of its bytes
//!   at once;
//! - `raw-bytes`: the bytes read back are the bytes written, with nothing added to them;
//! - `st_nlink`: the link count reports the second name.

use std::io;
use std::path::Path;

use super::Disk;
use super::layout::{make_dirs, objects_dir, stage_dir};

/// The bytes of the probe object.
const PROBE: &[u8] = b"tierstore local cache probe\n";

/// A file system that answered a question of the check wrongly, or a directory the check could
/// not use. The binary exits with status 3 for it, as for a tier that fails its self-test.
#[derive(Debug, thiserror::Error)]
pub enum LocalTierFailure {
    /// The directory could not be used for the probe.
    #[error("refusing to start: the local cache at {path}: {source}")]
    Unusable {
        /// The cache directory.
        path: String,
        /// What the system said.
        source: io::Error,
    },
    /// The file system answered a question wrongly.
    #[error(
        "refusing to start: the file system of the local cache at {path} failed the {check} check"
    )]
    Failed {
        /// The cache directory.
        path: String,
        /// The question.
        check: &'static str,
    },
}

/// Writes the probe object in `dir` and gives it a second name `second`, then reads both.
fn probe(disk: &dyn Disk, dir: &Path, second: &Path) -> io::Result<[(&'static str, bool); 4]> {
    std::fs::create_dir_all(dir)?;
    let first = dir.join("first");
    std::fs::write(&first, PROBE)?;
    disk.link(&first, second)?;
    let again = disk.link(&first, second);
    let create_only = again.is_err_and(|error| error.kind() == io::ErrorKind::AlreadyExists);
    // A file that cannot be read back is a question answered wrongly, not one not asked.
    let seen = std::fs::read(second).unwrap_or_default();
    let back = std::fs::read(&first).unwrap_or_default();
    let links = disk.links(&first).unwrap_or(0);
    Ok([
        ("create-only", create_only),
        ("atomic-visibility", seen == PROBE),
        ("raw-bytes", back == PROBE),
        ("st_nlink", links == 2),
    ])
}

/// Asks the file system under `root` the four questions. `run` is new for each start and names
/// the probe's directory.
///
/// # Errors
/// [`LocalTierFailure::Failed`] for the first question it answers wrongly, and
/// [`LocalTierFailure::Unusable`] if the probe could not be made.
pub fn check_local(disk: &dyn Disk, root: &Path, run: &str) -> Result<(), LocalTierFailure> {
    let path = root.display().to_string();
    let dir = stage_dir(root).join(format!("selfcheck-{run}"));
    let second = objects_dir(root).join(format!("selfcheck-{run}"));
    let answers = make_dirs(root, &objects_dir(root))
        .and_then(|()| probe(disk, &dir, &second))
        .map_err(|source| LocalTierFailure::Unusable {
            path: path.clone(),
            source,
        });
    // The first name is in the stage directory, which is emptied at start; the second is not,
    // so it is removed here.
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&second);
    let answers = answers?;
    match answers.iter().find(|(_, right)| !right) {
        Some((check, _)) => Err(LocalTierFailure::Failed { path, check }),
        None => Ok(()),
    }
}
