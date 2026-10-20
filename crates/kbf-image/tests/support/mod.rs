//! Shared by the integration tests of this crate.

#![allow(dead_code)]

pub mod registry;

use std::ops::Deref;
use std::path::Path;

use kbf_testkit::dir::TestDir;

/// A fresh, empty directory for one test, under Cargo's temporary directory, removed
/// (by [`force_remove`], since a test may leave directories nobody can write) when it is
/// dropped.
pub struct Scratch(TestDir);

impl Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Path {
        self.0.path()
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        self.0.path()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // The `TestDir` inside drops after this and finds nothing left to remove.
        force_remove(self.0.path());
    }
}

pub fn scratch(name: &str) -> Scratch {
    Scratch(TestDir::under(
        env!("CARGO_TARGET_TMPDIR"),
        &format!("kbf-image-{name}"),
    ))
}

/// Removes `dir`, first giving the owner every permission on every directory in it.
pub fn force_remove(dir: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    if std::fs::symlink_metadata(dir).is_err() {
        return;
    }
    let mut pending = vec![dir.to_owned()];
    while let Some(d) = pending.pop() {
        let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755));
        if let Ok(entries) = std::fs::read_dir(&d) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    pending.push(entry.path());
                }
            }
        }
    }
    let status = std::process::Command::new("rm")
        .arg("-rf")
        .arg(dir)
        .status()
        .expect("run rm");
    assert!(status.success(), "rm -rf {}", dir.display());
}
