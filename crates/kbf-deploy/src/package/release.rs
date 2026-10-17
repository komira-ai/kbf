//! The files `package postinst` takes from the package into the layout the updater uses
//! (design section 3.5): the offline-verification trust files, and the release the
//! package ships.
//!
//! - The trust files in `/usr/lib/kbf/libexec/` (the pinned `gh`, the trusted root, the
//!   default installer settings) are copied to `/usr/local/libexec/kbf-deploy/`, root's
//!   and read-only to everyone else, because that is the path offline verification
//!   reads. A file the package stopped shipping is not removed from the copy.
//! - The release in `/usr/lib/kbf/release/` (`kbf`, `units/`, `manifest.json`) is copied
//!   to `/usr/local/kbf/releases/<commit>/`, the commit being the 40 characters
//!   `manifest.json` names under `"commit"`. A release directory already there must hold
//!   the same files (a release is never replaced), as with `kbf-deploy host install`.
//! - `current` points at it only for a first install: no `current` link, and no
//!   `current` in the deploy state. Then the deploy state is written to say so. After
//!   that the updater owns `current` and `previous`, and this verb leaves both alone, so
//!   a package upgrade installs its release beside the others and activates nothing.
//!   One half-done first install is healed: a `current` link to this very release that
//!   the state does not know (the run died between the two writes) gets the state.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use kbf_host::exec::{Entry, Exec};
use kbf_host::path::{HostPath, Root};
use kbf_host::plan::Planner;
use kbf_host::step::{Owner, Perms};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::{PackageError, SHIPPED_LIBEXEC, SHIPPED_RELEASE, TRUST_DIR, path, read_failed, say};
use crate::host::CommitId;
use crate::host::layout;
use crate::host::release::{BINARY, DIR, ReleaseError, Source, UNIT};
use crate::host::state::State;
use crate::linux::host::{inspect, stat};

/// A file of the package's trust directory.
#[derive(Debug)]
pub struct TrustFile {
    pub name: String,
    pub bytes: Vec<u8>,
    pub perms: Perms,
}

/// The release the package ships: its commit and the files to copy.
#[derive(Debug)]
pub struct Shipped {
    pub commit: CommitId,
    pub source: Source,
}

/// `manifest.json`: only the commit is read; the rest belongs to the packaging.
#[derive(Deserialize)]
struct Manifest {
    commit: String,
}

fn utf8(name: &std::ffi::OsStr, full: &Path) -> Result<String, PackageError> {
    name.to_str()
        .map(str::to_owned)
        .ok_or_else(|| PackageError::NotUtf8(full.to_owned()))
}

/// The files of `/usr/lib/kbf/libexec/` under `root`, by name. Anything but a plain file
/// is refused: the package ships none, and a link would be followed by the copy.
pub fn read_trust(root: &Root) -> Result<Vec<TrustFile>, PackageError> {
    let dir = root.join(&path(SHIPPED_LIBEXEC));
    let mut files = Vec::new();
    for entry in fs::read_dir(&dir).map_err(read_failed(&dir))? {
        let file = entry.map_err(read_failed(&dir))?.path();
        let meta = fs::symlink_metadata(&file).map_err(read_failed(&file))?;
        if !meta.is_file() {
            return Err(PackageError::Shipped(file));
        }
        let name = utf8(
            file.file_name().expect("a directory entry has a name"),
            &file,
        )?;
        let executable = meta.permissions().mode() & 0o111 != 0;
        files.push(TrustFile {
            name,
            bytes: fs::read(&file).map_err(read_failed(&file))?,
            perms: Perms::new(if executable { 0o755 } else { 0o644 }),
        });
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(files)
}

/// The release in `/usr/lib/kbf/release/` under `root`.
pub fn read_shipped(root: &Root) -> Result<Shipped, PackageError> {
    let dir = root.join(&path(SHIPPED_RELEASE));
    let manifest = dir.join(layout::RELEASE_MANIFEST);
    let bytes = fs::read(&manifest).map_err(read_failed(&manifest))?;
    let bad = |why: String| PackageError::Manifest {
        path: manifest.clone(),
        why,
    };
    let Manifest { commit } = serde_json::from_slice(&bytes).map_err(|e| bad(e.to_string()))?;
    let commit = CommitId::new(&commit).map_err(|e| bad(e.to_string()))?;
    Ok(Shipped {
        commit,
        source: Source::read(&dir)?,
    })
}

/// `dir/name`.
fn child(dir: &str, name: &str) -> Result<HostPath, PackageError> {
    Ok(HostPath::new(&format!("{dir}/{name}"))?)
}

/// Plan the trust directory (root's, 0755) and each file in it.
pub fn plan_trust<E: Exec>(
    planner: &mut Planner<'_, E>,
    root: Owner,
    files: &[TrustFile],
) -> Result<(), PackageError> {
    planner.dir(path("/usr/local/libexec"), DIR, Some(root))?;
    planner.dir(path(TRUST_DIR), DIR, Some(root))?;
    for f in files {
        let dst = child(TRUST_DIR, &f.name)?;
        planner.file(dst, f.perms, Some(root), f.bytes.clone())?;
    }
    Ok(())
}

/// Plan the release directory with its files, then `current` for a first install. The
/// deploy state to write afterwards, when the verb changes what it says.
pub fn plan_release<E: Exec>(
    exec: &E,
    planner: &mut Planner<'_, E>,
    root: Owner,
    shipped: &Shipped,
    state: &State,
    out: &mut dyn Write,
) -> Result<Option<State>, PackageError> {
    let commit = &shipped.commit;
    let dir = layout::release_dir(commit);
    let units = layout::release_file(commit, layout::RELEASE_UNITS);
    let mut files = vec![(
        layout::release_file(commit, layout::RELEASE_BINARY),
        BINARY,
        &shipped.source.binary,
    )];
    for (name, bytes) in &shipped.source.units {
        files.push((child(units.as_str(), name)?, UNIT, bytes));
    }
    match inspect(exec, &dir)? {
        None => {}
        Some(Entry::Dir { .. }) => {
            for (path, _, want) in &files {
                let same = matches!(inspect(exec, path)?, Some(Entry::File { bytes, .. }) if bytes == **want);
                if !same {
                    return Err(ReleaseError::Differs { path: path.clone() }.into());
                }
            }
        }
        Some(_) => return Err(ReleaseError::NotADirectory(dir).into()),
    }
    let mut dirs = vec![
        path(layout::STATE_DIR),
        path(layout::KBF_DIR),
        path(layout::RELEASES),
        dir,
    ];
    if !shipped.source.units.is_empty() {
        dirs.push(units);
    }
    for d in dirs {
        planner.dir(d, DIR, Some(root))?;
    }
    for (path, perms, bytes) in files {
        planner.file(path, perms, Some(root), bytes.clone())?;
    }
    link(exec, planner, commit, state, out)
}

/// The `current` handshake (module docs): the deploy state to write, if any.
fn link<E: Exec>(
    exec: &E,
    planner: &mut Planner<'_, E>,
    commit: &CommitId,
    state: &State,
    out: &mut dyn Write,
) -> Result<Option<State>, PackageError> {
    let current = path(layout::CURRENT);
    let mine = layout::release_link(commit);
    let first = State {
        current: Some(commit.clone()),
        ..state.clone()
    };
    match (inspect(exec, &current)?, &state.current) {
        (None, None) => {
            say(
                out,
                &format!("current: a first install, {commit} becomes current"),
            )?;
            planner.symlink(current, mine)?;
            Ok(Some(first))
        }
        (None, Some(named)) => {
            say(
                out,
                &format!(
                    "current: the deploy state names {named} but the link is missing; \
                     left to the updater (kbf-deploy host status)"
                ),
            )?;
            Ok(None)
        }
        (Some(Entry::Link { target }), None) if target == mine => {
            say(
                out,
                &format!("current: already {commit}; writing the deploy state"),
            )?;
            Ok(Some(first))
        }
        (Some(_), _) => {
            say(out, "current: exists, left to the updater")?;
            Ok(None)
        }
    }
}

/// After the copy: the binary at the release directory has the shipped binary's sha256.
pub fn verify_copy<E: Exec>(exec: &E, shipped: &Shipped) -> Result<(), PackageError> {
    let bin = layout::release_file(&shipped.commit, layout::RELEASE_BINARY);
    let sha = |bytes: &[u8]| hex::encode(Sha256::digest(bytes));
    let got = match inspect(exec, &bin)? {
        Some(Entry::File { bytes, .. }) => sha(&bytes),
        _ => "none (no file)".to_owned(),
    };
    let want = sha(&shipped.source.binary);
    if got == want {
        Ok(())
    } else {
        Err(PackageError::Copy {
            path: bin,
            want,
            got,
        })
    }
}

/// Whether anything is at `path` (a link is not followed).
pub(crate) fn present<E: Exec>(exec: &E, path: &HostPath) -> Result<bool, PackageError> {
    Ok(stat(exec, path)?.is_some())
}

#[cfg(test)]
#[path = "release_tests.rs"]
mod tests;
