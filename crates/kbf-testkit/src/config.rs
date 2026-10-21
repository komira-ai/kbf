//! The settings file of a `kbf` child a test starts.
//!
//! `kbf` reads `/etc/kbf/kbf.toml` when it is given no `--config` and that file exists, so
//! a child started without the flag behaves by the host it runs on: on a host that has
//! kbf installed the file is there (and may be unreadable to the test's user), on a host
//! that has not it is absent, and the same test passes on one and fails on the other.
//! Every test that starts a `kbf` gives it the flag [`flag`] returns.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

/// The file name under the directory [`flag`] is given.
pub const FILE: &str = "kbf.toml";

/// Writes an empty `kbf.toml` (mode 0600, as the settings file must not be writable by
/// group or other) into `dir`, and returns `--config=<that file>`: the flag that makes a
/// `kbf` child read it and nothing else. The settings of a test come from its flags.
///
/// # Panics
/// If the file cannot be written (a test cannot go on without it).
#[must_use]
pub fn flag(dir: &Path) -> String {
    flag_with(dir, "")
}

/// [`flag`] with `text` in the file: the settings of a child that is started from a
/// `kbf.toml` instead of from flags (the farm's processes, `kbf-harness`).
///
/// # Panics
/// If the file cannot be written (a test cannot go on without it).
#[must_use]
pub fn flag_with(dir: &Path, text: &str) -> String {
    let path = dir.join(FILE);
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .expect("write the kbf.toml");
    file.write_all(text.as_bytes()).expect("write the kbf.toml");
    format!("--config={}", path.display())
}
