//! `record-release --commit <sha>`: make a release the operator placed by hand the host's
//! first release, so that `status.json` can name it and the frontend proxy can start it after
//! a reboot.
//!
//! The fetch and verify pipeline ([`crate::stage`]) installs a release and writes its
//! `VERIFIED` marker; a release put on the host some other way has neither the marker nor a
//! `state.json`. This verb checks what can be checked on the files and then vouches:
//!
//! - the release directory and `kbf` are plain, owned by the user who runs the verb (root) and
//!   not writable by group or others, and `kbf` and the directory are executable by others (the
//!   frontend child runs as the proxy's user, not root, so the owner's bit does not count);
//! - `kbf` starts with the header of the executable this host runs ([`BinaryKind`]), not a
//!   script or another platform's file.
//!
//! It does not fetch, and it does **not** check an attestation or run the binary: the
//! operator who names the commit says it is that commit. With `--apply` it writes `VERIFIED`
//! (the marker says the operator recorded it, not that the pipeline verified it: the proxy
//! reads only that the marker exists) and makes the release `current` through
//! [`activate::first`], which refuses a host that has another current release. A repeat for
//! the same commit changes nothing. `status.json` follows from the installer service's next
//! cycle.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use kbf_host::exec::Exec;

use crate::CommitId;
use crate::activate;
use crate::binary::{Arch, BinaryKind};
use crate::layout::{Layout, RELEASE_BINARY};
use crate::release::ReleaseError;
use crate::stage::VERIFIED;

/// Why the verb refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    /// `releases/<sha>` or `kbf` is not what a release is.
    #[error("{path}: {why}")]
    Refused { path: String, why: &'static str },
    #[error("cannot read {path}: {source}")]
    Read { path: String, source: io::Error },
    #[error(transparent)]
    Release(#[from] ReleaseError),
    #[error("cannot write {path}: {source}")]
    Write { path: String, source: io::Error },
}

/// The frontend child runs as the proxy's user, which is not root, and the files are root's (the
/// check requires it): so only the execute bit for others lets it start.
const NOT_RUNNABLE: &str = "is not executable by the frontend's user (not root); fix: chmod 755 it";

fn refused(path: &Path, why: &'static str) -> RecordError {
    RecordError::Refused {
        path: path.display().to_string(),
        why,
    }
}

/// Check the release directory of `commit` as `euid` and, when `apply`, record it.
///
/// # Errors
/// The files are not a release, the host has another current release, or a write failed.
pub fn record<E: Exec>(
    exec: &mut E,
    host: &Layout,
    euid: u32,
    commit: &CommitId,
    apply: bool,
    out: &mut dyn Write,
) -> Result<(), RecordError> {
    let dir = host.join(&host.release_dir(commit));
    check(
        &dir,
        euid,
        BinaryKind::wanted(host.platform(), Arch::current()),
    )?;
    if !apply {
        let _ = writeln!(
            out,
            "DRY-RUN: record {commit} as the host's first release (the operator vouches for it; \
             nothing is fetched or attested)"
        );
        return Ok(());
    }
    // The state first: a refusal (another release is current) leaves no marker behind, and a
    // marker that fails to write is written by the repeat.
    activate::first(exec, host, commit, out)?;
    mark(&dir, commit)?;
    let _ = writeln!(
        out,
        "recorded {commit}: the installer service writes status.json from it on its next cycle"
    );
    Ok(())
}

/// The structure of the release at `dir`.
fn check(dir: &Path, euid: u32, wanted: BinaryKind) -> Result<(), RecordError> {
    let plain = |path: &Path, dir_wanted: bool| -> Result<fs::Metadata, RecordError> {
        let meta = fs::symlink_metadata(path).map_err(|source| RecordError::Read {
            path: path.display().to_string(),
            source,
        })?;
        let kind_ok = if dir_wanted {
            meta.is_dir()
        } else {
            meta.is_file()
        };
        if !kind_ok {
            return Err(refused(
                path,
                if dir_wanted {
                    "is not a directory"
                } else {
                    "is not a plain file"
                },
            ));
        }
        if meta.uid() != euid {
            return Err(refused(path, "is not owned by the user running this"));
        }
        if meta.mode() & 0o022 != 0 {
            return Err(refused(path, "is writable by group or others"));
        }
        Ok(meta)
    };
    let release = plain(dir, true)?;
    if release.mode() & 0o001 == 0 {
        return Err(refused(dir, NOT_RUNNABLE));
    }
    let binary = dir.join(RELEASE_BINARY);
    let meta = plain(&binary, false)?;
    if meta.mode() & 0o001 == 0 {
        return Err(refused(&binary, NOT_RUNNABLE));
    }
    let mut header = [0_u8; 64];
    let mut file = fs::File::open(&binary).map_err(|source| RecordError::Read {
        path: binary.display().to_string(),
        source,
    })?;
    let read = file.read(&mut header).map_err(|source| RecordError::Read {
        path: binary.display().to_string(),
        source,
    })?;
    if BinaryKind::of(&header[..read]) != wanted {
        return Err(refused(&binary, "is not the executable this host runs"));
    }
    match fs::symlink_metadata(dir.join(VERIFIED)) {
        Ok(m) if !m.is_file() => Err(refused(&dir.join(VERIFIED), "is not a plain file")),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(RecordError::Read {
            path: dir.join(VERIFIED).display().to_string(),
            source,
        }),
    }
}

/// Write the `VERIFIED` marker unless it is there.
fn mark(dir: &Path, commit: &CommitId) -> Result<(), RecordError> {
    let path = dir.join(VERIFIED);
    let made = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&path);
    let write = |source| RecordError::Write {
        path: path.display().to_string(),
        source,
    };
    match made {
        Ok(mut file) => file
            .write_all(format!("{commit}\n").as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(write),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(write(e)),
    }
}

#[cfg(test)]
#[path = "record_release_tests.rs"]
mod tests;
