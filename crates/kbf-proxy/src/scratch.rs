//! A scratch directory and private files for the unit tests of the modules that read files.
//! Beside the test binary, so nothing is written under `/tmp`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// A fresh empty directory for one test.
pub(crate) fn scratch(name: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps directory")
        .join("kbf-proxy-unit")
        .join(format!("{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// Writes `content` to `path` with `mode`.
pub(crate) fn write_file(path: &Path, content: &[u8], mode: u32) {
    // A read-only file of an earlier write cannot be written over.
    let _ = std::fs::remove_file(path);
    std::fs::write(path, content).expect("write the file");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}
