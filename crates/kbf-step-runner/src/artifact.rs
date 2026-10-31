//! The verified artifact: a staged release tree and the digest it had when the updater verified
//! it. Nothing from the tree runs unless it still has that digest, and the check is made again
//! before every step, resumed ones included (docs/design/host-updates.md, 3.3 and 3.5).
//!
//! The staged tree is root-owned and read-only in root-owned parents (3.4.1), so it cannot be
//! swapped between this check and the exec; the check is what notices a tree that was changed
//! anyway (a bad restore, a disk fault, a root that was tricked), and the runner treats it as a
//! `halt`.

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

/// Why a staged release cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    #[error("cannot read the staged release: {0}")]
    Io(#[from] io::Error),
    #[error("the staged release holds {0:?}, which is neither a file, a directory nor a link")]
    Unsupported(PathBuf),
    #[error("the staged release changed after it was verified: it was {expected}, it is {found}")]
    Changed { expected: String, found: String },
    #[error(
        "{0:?} is not a directory owned by the updater or root and closed to group and others, or is a link"
    )]
    Untrusted(PathBuf),
    #[error("the hook path {0:?} is not a relative path of plain names")]
    BadHookPath(PathBuf),
    #[error("the hook {0:?} is not a regular file of the staged release")]
    HookMissing(PathBuf),
}

/// A staged release and its digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Staged {
    /// The absolute path of the tree.
    pub dir: PathBuf,
    /// The lower-case hex sha256 of the tree ([`tree_digest`]) at the time it was verified.
    pub digest: String,
}

impl Staged {
    /// The tree at `dir` as it is now. Called by the step that verified the release, once, right
    /// after the verification and before anything else touches the tree.
    pub fn seal(dir: &Path) -> Result<Staged, ArtifactError> {
        Ok(Staged {
            dir: dir.to_path_buf(),
            digest: tree_digest(dir)?,
        })
    }

    /// Checks the tree still has its digest and returns the hook executable inside it.
    ///
    /// **What the launcher may rely on:** the returned path is a regular file of a tree that was
    /// not changed since it was sealed, and the tree and every directory above it were, at this
    /// check, real directories (no link) owned by the running user or root and closed to group
    /// and others. The updater's staged trees are root-owned, read-only and in root-owned
    /// parents, so the digest and the place together mean the file cannot be swapped before the
    /// exec. A launcher that runs the path must run it from here, not from a path it built.
    ///
    /// `hook` is the declaration's `binary`: relative, plain names only, a regular file of the
    /// tree (not a link, so it cannot lead out of it).
    pub fn check(&self, hook: &Path) -> Result<PathBuf, ArtifactError> {
        check_place(&self.dir)?;
        let found = tree_digest(&self.dir)?;
        if found != self.digest {
            return Err(ArtifactError::Changed {
                expected: self.digest.clone(),
                found,
            });
        }
        if hook.as_os_str().is_empty() || !hook.components().all(is_plain) {
            return Err(ArtifactError::BadHookPath(hook.to_path_buf()));
        }
        let path = self.dir.join(hook);
        if !fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file()) {
            return Err(ArtifactError::HookMissing(hook.to_path_buf()));
        }
        Ok(path)
    }
}

/// The tree and every directory above it are real directories (not links) owned by the running
/// user or root and not writable by group or others.
fn check_place(dir: &Path) -> Result<(), ArtifactError> {
    let me = rustix::process::geteuid().as_raw();
    let mut at = PathBuf::new();
    for part in dir.components() {
        at.push(part);
        let meta = fs::symlink_metadata(&at)?;
        // One comparison; a link is not a directory here, so a linked component fails it.
        let sound = (
            meta.is_dir(),
            [me, 0].contains(&meta.uid()),
            meta.mode() & 0o022 == 0,
        );
        if sound != (true, true, true) {
            return Err(ArtifactError::Untrusted(at));
        }
    }
    Ok(())
}

fn is_plain(component: Component<'_>) -> bool {
    matches!(component, Component::Normal(_))
}

/// The sha256 of a tree: every name, kind, mode and content below `dir`, in name order, links
/// by their target and never followed. Lower-case hex.
pub fn tree_digest(dir: &Path) -> Result<String, ArtifactError> {
    let mut hasher = Sha256::new();
    hash_dir(&mut hasher, dir, Path::new(""))?;
    Ok(hex(&hasher.finalize()))
}

fn hash_dir(hasher: &mut Sha256, dir: &Path, rel: &Path) -> Result<(), ArtifactError> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir)? {
        names.push(entry?.file_name());
    }
    names.sort();
    for name in names {
        let path = dir.join(&name);
        let rel = rel.join(&name);
        let meta = fs::symlink_metadata(&path)?;
        let mode = meta.permissions().mode() & 0o7777;
        let kind = meta.file_type();
        if kind.is_dir() {
            put(hasher, format!("d\0{}\0{mode}\0", rel.display()).as_bytes());
            hash_dir(hasher, &path, &rel)?;
        } else if kind.is_file() {
            put(
                hasher,
                format!("f\0{}\0{mode}\0{}\0", rel.display(), meta.size()).as_bytes(),
            );
            hasher.update(fs::read(&path)?);
        } else if kind.is_symlink() {
            let target = fs::read_link(&path)?;
            put(
                hasher,
                format!("l\0{}\0{}\0", rel.display(), target.display()).as_bytes(),
            );
        } else {
            return Err(ArtifactError::Unsupported(rel));
        }
    }
    Ok(())
}

/// One framed record: its length, then its bytes, so no two trees hash the same stream.
fn put(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}
