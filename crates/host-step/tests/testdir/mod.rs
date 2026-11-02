//! [`TestDir`]: a scratch directory a test owns, removed when it is dropped.
//!
//! The directory is `<root>/<name>-<pid>-<n>` (unique per test: name, process id and a
//! counter), exists from [`TestDir::under`] on, and is removed with everything in it on drop.
//! Tests pass `env!("CARGO_TARGET_TMPDIR")` as the root, so nothing lands in the system
//! temporary directory. This crate keeps its own copy so that it depends on no other crate of
//! the workspace, even for its tests.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// A directory removed (with everything in it) on drop.
#[derive(Debug)]
pub struct TestDir {
    path: PathBuf,
}

impl TestDir {
    /// A new, empty directory `<root>/<name>-<pid>-<n>`, making `root` if needed.
    ///
    /// # Panics
    /// If the directory cannot be made (a test cannot go on without it).
    #[must_use]
    pub fn under(root: impl AsRef<Path>, name: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = root
            .as_ref()
            .join(format!("{name}-{}-{n}", std::process::id()));
        // Absent unless an earlier process of this pid left it.
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path)
            .unwrap_or_else(|e| panic!("make test dir {}: {e}", path.display()));
        Self { path }
    }

    /// The directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        match std::fs::remove_dir_all(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("could not remove test dir {}: {e}", self.path.display()),
        }
    }
}
