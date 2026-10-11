//! The check that a binary run as root can be replaced only by root.
//!
//! Run as root from a path some user can write (a download directory, a checkout), the
//! binary is that user's code with root's rights. So before applying as root, the
//! binary and every directory above it must be owned by root and writable by neither
//! group nor others.

use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Why the running binary is not trusted to act as root.
#[derive(Debug, thiserror::Error)]
pub enum Untrusted {
    #[error("{path} is owned by uid {uid}, not root")]
    NotRootOwned { path: PathBuf, uid: u32 },
    #[error("{path} is writable by group or others (mode {mode:04o})")]
    Writable { path: PathBuf, mode: u32 },
    #[error("cannot check {path}: {source}")]
    Unreadable { path: PathBuf, source: io::Error },
}

/// Checks `path` (canonical: absolute, no links) and each directory above it, up to
/// `/`. `lookup` gives a path's owner uid and mode without following a link.
pub fn check_ancestry(
    path: &Path,
    lookup: impl Fn(&Path) -> io::Result<(u32, u32)>,
) -> Result<(), Untrusted> {
    for p in path.ancestors() {
        let (uid, mode) = lookup(p).map_err(|source| Untrusted::Unreadable {
            path: p.to_owned(),
            source,
        })?;
        if uid != 0 {
            return Err(Untrusted::NotRootOwned {
                path: p.to_owned(),
                uid,
            });
        }
        if mode & 0o022 != 0 {
            return Err(Untrusted::Writable {
                path: p.to_owned(),
                mode: mode & 0o7777,
            });
        }
    }
    Ok(())
}

fn lookup(p: &Path) -> io::Result<(u32, u32)> {
    let meta = std::fs::symlink_metadata(p)?;
    Ok((meta.uid(), meta.mode()))
}

/// [`check_ancestry`] on the running binary, after resolving every link in its path.
pub fn check_running_binary() -> Result<(), Untrusted> {
    check_resolved(
        std::env::current_exe().and_then(std::fs::canonicalize),
        lookup,
    )
}

fn check_resolved(
    exe: io::Result<PathBuf>,
    lookup: impl Fn(&Path) -> io::Result<(u32, u32)>,
) -> Result<(), Untrusted> {
    match exe {
        Ok(exe) => check_ancestry(&exe, lookup),
        Err(source) => Err(Untrusted::Unreadable {
            path: PathBuf::from("the running binary"),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn tree(entries: &[(&str, u32, u32)]) -> impl Fn(&Path) -> io::Result<(u32, u32)> {
        let map: BTreeMap<PathBuf, (u32, u32)> = entries
            .iter()
            .map(|(p, uid, mode)| (PathBuf::from(p), (*uid, *mode)))
            .collect();
        move |p| {
            map.get(p)
                .copied()
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        }
    }

    /// Catches a check that looks at the binary alone (a user-owned or world-writable
    /// directory above it lets that user swap the binary), one that stops before `/`,
    /// and one that reads the owner but not the group and other write bits.
    #[test]
    fn every_directory_up_to_the_root_is_checked() {
        let good = [
            ("/", 0, 0o40755),
            ("/usr", 0, 0o40755),
            ("/usr/local", 0, 0o40755),
            ("/usr/local/bin", 0, 0o40755),
            ("/usr/local/bin/kbf-deploy", 0, 0o100755),
        ];
        let exe = Path::new("/usr/local/bin/kbf-deploy");
        check_ancestry(exe, tree(&good)).expect("a tree root owns alone passes");

        type Want = fn(&Untrusted) -> bool;
        let cases: [(usize, u32, u32, &str, Want); 6] = [
            (4, 501, 0o100755, "/usr/local/bin/kbf-deploy", |e| {
                matches!(e, Untrusted::NotRootOwned { uid: 501, .. })
            }),
            (4, 0, 0o100775, "/usr/local/bin/kbf-deploy", |e| {
                matches!(e, Untrusted::Writable { mode: 0o775, .. })
            }),
            (3, 0, 0o40757, "/usr/local/bin", |e| {
                matches!(e, Untrusted::Writable { mode: 0o757, .. })
            }),
            (2, 501, 0o40755, "/usr/local", |e| {
                matches!(e, Untrusted::NotRootOwned { .. })
            }),
            (1, 0, 0o41777, "/usr", |e| {
                matches!(e, Untrusted::Writable { mode: 0o1777, .. })
            }),
            (0, 0, 0o40775, "/", |e| {
                matches!(e, Untrusted::Writable { .. })
            }),
        ];
        for (at, uid, mode, path, want) in cases {
            let mut entries = good;
            entries[at] = (entries[at].0, uid, mode);
            let err = check_ancestry(exe, tree(&entries)).expect_err(path);
            assert!(want(&err), "{path}: {err}");
            assert!(err.to_string().starts_with(path), "{err}");
        }

        let missing = &good[1..];
        let err = check_ancestry(exe, tree(missing)).expect_err("/ cannot be read");
        assert_eq!(err.to_string(), "cannot check /: entity not found");
    }

    /// The test binary sits in a target directory the test user owns, so the real
    /// lookup must refuse it: catches a check that never looks at the file system.
    #[test]
    fn a_binary_in_a_user_owned_tree_is_refused() {
        let err = check_running_binary().expect_err("the test binary is user-owned");
        assert!(matches!(err, Untrusted::NotRootOwned { .. }), "{err}");
    }

    /// Catches a binary whose path cannot be resolved being let through unchecked.
    #[test]
    fn an_unresolvable_binary_is_refused() {
        let gone = Err(io::Error::from(io::ErrorKind::NotFound));
        let err = check_resolved(gone, lookup).expect_err("no path");
        assert!(matches!(err, Untrusted::Unreadable { .. }), "{err}");
    }
}
