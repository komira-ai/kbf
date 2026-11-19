//! The files a run writes: beside the target, synced, and renamed over it.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

/// Writes `value` as JSON to `path`, so that a reader finds the old file or the new one and
/// never half of either.
pub fn write_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    write_atomic(path, &bytes)
}

/// Writes `bytes` to a file beside `path`, syncs it, and renames it over `path`.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    let temp = PathBuf::from(name);
    let mut file = File::create(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    // The rename is durable once the directory that holds the name is.
    let parent = path.parent().filter(|dir| !dir.as_os_str().is_empty());
    File::open(parent.unwrap_or(Path::new(".")))?.sync_all()
}
