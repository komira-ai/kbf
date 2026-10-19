//! [`TestDir`]: a scratch directory a test owns, removed when it is dropped.
//!
//! Tests used to make `<root>/<name>-<pid>` and never remove it, so every run left a
//! directory per test behind. A `TestDir` is unique per test (name, process id and a
//! counter), exists from [`TestDir::under`] on, and is removed on drop, except when the
//! thread is panicking and `KBF_KEEP_TMP` is set (a failed test's files are then left
//! for a person to read; the path is printed to stderr).
//!
//! Drop order matters when a test starts processes that write into the directory: a
//! `TestDir` drops after every local declared after it, so declare it first and the
//! processes (their guards reaped on drop) are gone before the directory is removed.

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// The variable that keeps the directory of a test that panicked.
pub const KEEP_ENV: &str = "KBF_KEEP_TMP";

/// A directory removed (with everything in it) on drop.
#[derive(Debug)]
pub struct TestDir {
    path: PathBuf,
}

impl TestDir {
    /// A new, empty directory `<root>/<name>-<pid>-<n>`, making `root` if needed.
    /// Integration tests pass `env!("CARGO_TARGET_TMPDIR")`; a unit test, which gets no
    /// such variable, passes [`std::env::temp_dir`].
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

impl Deref for TestDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        if std::thread::panicking() && std::env::var_os(KEEP_ENV).is_some() {
            eprintln!("{KEEP_ENV} set: keeping {}", self.path.display());
            return;
        }
        match std::fs::remove_dir_all(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("could not remove test dir {}: {e}", self.path.display()),
        }
    }
}
