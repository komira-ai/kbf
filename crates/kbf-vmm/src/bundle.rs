//! The VM bundle: the four files a macOS guest boots from, and cloning one.
//!
//! A bundle is a directory with `disk.img` (the guest's disk, a raw image), `aux.img`
//! (auxiliary storage, the guest's boot state), `hardware-model` and `machine-identifier`
//! (the data representations of `VZMacHardwareModel` and `VZMacMachineIdentifier`).
//! Cloning uses `clonefile(2)`, so the source and the destination must be on one APFS
//! volume, and a clone costs no disk until the guest writes.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::system::System;

pub const DISK: &str = "disk.img";
pub const AUX: &str = "aux.img";
pub const HARDWARE_MODEL: &str = "hardware-model";
pub const MACHINE_ID: &str = "machine-identifier";

/// The four file names, in the order they are cloned.
pub const FILES: [&str; 4] = [DISK, AUX, HARDWARE_MODEL, MACHINE_ID];

/// What a clone does with the machine identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    /// Keep the base's identifier: every clone of one base has the same one.
    Keep,
    /// Give the clone a new identifier on a copy of the base's auxiliary storage.
    Fresh,
}

/// Why a bundle could not be used or cloned.
#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error(
        "{dir} is not a prepared base bundle: {file} is missing (build one with `kbf-vmm image base`)"
    )]
    Missing { dir: PathBuf, file: &'static str },
    #[error("clonefile {src} -> {dst}: {source}")]
    Clone {
        src: PathBuf,
        dst: PathBuf,
        source: io::Error,
    },
    #[error("clear the immutable flag on {path}: {source}")]
    Unflag { path: PathBuf, source: io::Error },
    #[error("a new machine identifier: {0}")]
    NewIdentity(String),
    #[error("write {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
    #[error("{0} already exists: a clone never overwrites")]
    Exists(PathBuf),
}

/// Checks that `dir` holds the four files.
///
/// # Errors
/// A file is missing.
pub fn check(dir: &Path) -> Result<(), BundleError> {
    for file in FILES {
        if !dir.join(file).is_file() {
            return Err(BundleError::Missing {
                dir: dir.to_owned(),
                file,
            });
        }
    }
    Ok(())
}

/// Clones the bundle `base` into the new directory `dest`, returning how long the clone
/// calls took (the parent directory of `dest` must exist).
///
/// # Errors
/// `base` is not a bundle, `dest` exists, a `clonefile` fails (another volume, not APFS)
/// or the new identifier cannot be made or written.
pub fn clone_bundle(
    sys: &dyn System,
    base: &Path,
    dest: &Path,
    identity: Identity,
) -> Result<Duration, BundleError> {
    check(base)?;
    if dest.exists() {
        return Err(BundleError::Exists(dest.to_owned()));
    }
    std::fs::create_dir(dest).map_err(|source| BundleError::Write {
        path: dest.to_owned(),
        source,
    })?;
    let started = Instant::now();
    for file in FILES {
        if identity == Identity::Fresh && file == MACHINE_ID {
            let bytes = sys
                .new_machine_identifier()
                .map_err(BundleError::NewIdentity)?;
            let path = dest.join(file);
            std::fs::write(&path, bytes).map_err(|source| BundleError::Write { path, source })?;
            continue;
        }
        let (src, dst) = (base.join(file), dest.join(file));
        sys.clonefile(&src, &dst)
            .map_err(|source| BundleError::Clone {
                src: src.clone(),
                dst: dst.clone(),
                source,
            })?;
        // A clone of a sealed base keeps `uchg` and the guest could not write its disk.
        sys.clear_immutable(&dst)
            .map_err(|source| BundleError::Unflag { path: dst, source })?;
    }
    Ok(started.elapsed())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeSystem, holds, tmp, work_with_base};

    /// Catches a bundle check that passes with a file missing, and an error that does not
    /// name the file (so an operator cannot tell what to build).
    #[test]
    fn a_bundle_needs_all_four_files() {
        let work = work_with_base("bundle-check");
        assert!(check(&work.join("base")).is_ok());
        std::fs::remove_file(work.join("base").join(AUX)).expect("rm");
        let err = check(&work.join("base"))
            .expect_err("missing aux")
            .to_string();
        assert!(
            holds(&[err.contains("aux.img"), err.contains("image base")]),
            "{err}"
        );
        assert!(check(&tmp("bundle-empty")).is_err());
    }

    /// Catches a clone of a sealed base (root's, `uchg`) left immutable, so the guest
    /// could not write its disk: every cloned file is unflagged right after `clonefile`,
    /// and the one that keeps its flag fails the clone naming the path. A fresh identity
    /// is written, not cloned, so it has no flag to clear.
    #[test]
    fn every_cloned_file_loses_the_immutable_flag_of_a_sealed_base() {
        let work = work_with_base("bundle-unflag");
        let sys = FakeSystem::new();
        let dest = work.join("c1");
        clone_bundle(&sys, &work.join("base"), &dest, Identity::Keep).expect("clone");
        let want: Vec<PathBuf> = FILES.iter().map(|f| dest.join(f)).collect();
        assert_eq!(*sys.unflagged.borrow(), want);
        sys.unflagged.borrow_mut().clear();
        let fresh = work.join("c2");
        clone_bundle(&sys, &work.join("base"), &fresh, Identity::Fresh).expect("clone");
        assert_eq!(sys.unflagged.borrow().len(), 3);
        assert!(!sys.unflagged.borrow().contains(&fresh.join(MACHINE_ID)));
        sys.unflag_fails.set(true);
        let err = clone_bundle(&sys, &work.join("base"), &work.join("c3"), Identity::Keep)
            .expect_err("unflag");
        assert!(matches!(err, BundleError::Unflag { .. }));
        let text = err.to_string();
        assert!(text.contains("c3"), "{text}");
        assert!(text.contains("EPERM"), "{text}");
    }

    /// Catches a keep-identity clone that regenerates the identifier (two clones would
    /// then differ) or skips a file, and a clone that is not made file by file.
    #[test]
    fn keeping_the_identity_clones_all_four_files() {
        let work = work_with_base("bundle-keep");
        let sys = FakeSystem::new();
        let dest = work.join("c1");
        clone_bundle(&sys, &work.join("base"), &dest, Identity::Keep).expect("clone");
        assert_eq!(sys.cloned.borrow().len(), 4);
        for f in FILES {
            assert_eq!(
                std::fs::read(dest.join(f)).expect("read"),
                format!("contents of {f}").into_bytes()
            );
        }
    }

    /// Catches a fresh-identity clone that copies the base's identifier or clones the
    /// auxiliary storage not at all: the identifier is new, the rest is cloned.
    #[test]
    fn a_fresh_identity_replaces_only_the_identifier() {
        let work = work_with_base("bundle-fresh");
        let sys = FakeSystem::new();
        let dest = work.join("c1");
        clone_bundle(&sys, &work.join("base"), &dest, Identity::Fresh).expect("clone");
        assert_eq!(sys.cloned.borrow().len(), 3);
        assert_eq!(
            std::fs::read(dest.join(MACHINE_ID)).expect("id"),
            b"fresh-id"
        );
        assert_eq!(
            std::fs::read(dest.join(AUX)).expect("aux"),
            b"contents of aux.img"
        );
    }

    /// Catches each failure being swallowed: a base that is not a bundle, a destination
    /// that exists, a clonefile error, a refused identifier, an unwritable identifier.
    #[test]
    fn clone_failures_are_reported() {
        let work = work_with_base("bundle-fail");
        let base = work.join("base");
        let sys = FakeSystem::new();
        assert!(matches!(
            clone_bundle(&sys, &work.join("nope"), &work.join("d"), Identity::Keep),
            Err(BundleError::Missing { .. })
        ));
        std::fs::create_dir(work.join("taken")).expect("mkdir");
        assert!(matches!(
            clone_bundle(&sys, &base, &work.join("taken"), Identity::Keep),
            Err(BundleError::Exists(_))
        ));
        *sys.clone_fails_on.borrow_mut() = Some("hardware-model");
        let err = clone_bundle(&sys, &base, &work.join("d2"), Identity::Keep).expect_err("clone");
        assert!(matches!(err, BundleError::Clone { .. }));
        assert!(err.to_string().contains("EXDEV"), "{err}");
        *sys.clone_fails_on.borrow_mut() = None;
        *sys.identity.borrow_mut() = Err("framework said no".into());
        let err = clone_bundle(&sys, &base, &work.join("d3"), Identity::Fresh).expect_err("id");
        assert!(err.to_string().contains("framework said no"), "{err}");
        *sys.identity.borrow_mut() = Ok(vec![1]);
        // The destination's parent does not exist: the directory cannot be made.
        let err = clone_bundle(&sys, &base, &work.join("no/such/parent"), Identity::Keep)
            .expect_err("mkdir");
        assert!(matches!(err, BundleError::Write { .. }));
        // The identifier cannot be written because its path is a directory.
        *sys.after_clone.borrow_mut() = Some(Box::new(|dst: &Path| {
            if dst.file_name().is_some_and(|n| n == HARDWARE_MODEL) {
                std::fs::create_dir(dst.with_file_name(MACHINE_ID)).expect("dir in the way");
            }
        }));
        let err = clone_bundle(&sys, &base, &work.join("d4"), Identity::Fresh).expect_err("write");
        assert!(matches!(err, BundleError::Write { .. }), "{err}");
    }
}
