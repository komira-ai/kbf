//! The declaration a release carries, installed only through the gate
//! (docs/design/host-updates.md, 3.3).
//!
//! A verified release may carry `updater.d/<service>.toml`. Before `prepare` the root part reads
//! it from the verified tree and asks `update_rights::gate` about it against the active
//! declaration: one the operator approved (by digest), or one that asks for nothing more, is
//! installed; anything else is `Deferred` with the reason `rights_change` and nothing runs. With
//! no active declaration, a first install is always deferred until approved, whoever the hook
//! runs as. A release that carries none, or the one that is active, changes nothing.
//!
//! Installing keeps the active declaration (or the fact that there was none) in the service's
//! state directory first, so that an update that then fails can put it back ([`restore`]),
//! even after a crash between the two.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use kbf_step_runner::{ArtifactError, Staged};
use update_rights::{Approvals, Declaration, Deferral, Digest, Gate, Loaded, gate};

/// The active declaration of an update in progress, kept to be put back.
pub const PREVIOUS: &str = "declaration.previous";
/// Kept instead when the update installed the first declaration of its service.
pub const FIRST: &str = "declaration.first";

/// What the gate said of the declaration a release carries.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// The release carries none, or the active one: nothing changes.
    Unchanged,
    /// It was installed; the active one is kept to be restored.
    Installed,
    /// It asks for more than the active one, or is a first install, and is not approved.
    Deferred(Deferral),
}

/// The declaration `service`'s release carries, read from the verified tree.
///
/// # Errors
/// A tree that is not the one verified, or a file that cannot be read.
pub fn carried(staged: &Staged, service: &str) -> Result<Option<Vec<u8>>, String> {
    let inside = Path::new("updater.d").join(format!("{service}.toml"));
    match staged.check(&inside) {
        Ok(path) => fs::read(&path).map(Some).map_err(|e| e.to_string()),
        Err(ArtifactError::HookMissing(_)) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Where the declarations live and who owns them.
pub struct Place<'a> {
    pub declarations: &'a Path,
    pub approvals: &'a Path,
    pub trusted_uid: u32,
    /// The root part's own directory for the service.
    pub state: &'a Path,
}

impl Place<'_> {
    fn file(&self, service: &str) -> PathBuf {
        self.declarations.join(format!("{service}.toml"))
    }
}

/// Decides on `bytes`, the declaration a release of `service` carries, and installs it when the
/// gate says so.
///
/// # Errors
/// A declaration that is not one, an approval file that cannot be trusted, or a write that
/// failed.
pub fn decide(
    place: &Place<'_>,
    loaded: &Loaded,
    service: &str,
    bytes: &[u8],
) -> Result<Decision, String> {
    let digest = Digest::of(bytes);
    let active = loaded.services.get(service);
    if active.map(|declared| declared.digest()) == Some(&digest) {
        return Ok(Decision::Unchanged);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "the declaration is not UTF-8")?;
    let incoming = Declaration::parse(text).map_err(|e| format!("the declaration: {e}"))?;
    if incoming.name != service {
        return Err(format!(
            "the declaration names {:?}, not {service:?}",
            incoming.name
        ));
    }
    let approvals =
        Approvals::read(place.approvals, place.trusted_uid).map_err(|e| e.to_string())?;
    let verdict = gate(
        active.map(|declared| declared.declaration()),
        &incoming,
        &digest,
        &approvals,
    );
    if let Gate::Deferred(deferral) = verdict {
        return Ok(Decision::Deferred(deferral));
    }
    let kept = match active {
        Some(declared) => (
            PREVIOUS,
            fs::read(declared.file()).map_err(|e| e.to_string())?,
        ),
        None => (FIRST, Vec::new()),
    };
    write_file(place.state, kept.0, &kept.1, 0o600).map_err(|e| e.to_string())?;
    write_file(place.declarations, &format!("{service}.toml"), bytes, 0o644)
        .map_err(|e| e.to_string())?;
    Ok(Decision::Installed)
}

/// Puts back the declaration an update installed, if it installed one: the one that was active,
/// or none for a first install. `true` when it changed the directory of declarations.
///
/// # Errors
/// A write that failed.
pub fn restore(place: &Place<'_>, service: &str) -> io::Result<bool> {
    let previous = place.state.join(PREVIOUS);
    let first = place.state.join(FIRST);
    if let Ok(bytes) = fs::read(&previous) {
        write_file(
            place.declarations,
            &format!("{service}.toml"),
            &bytes,
            0o644,
        )?;
        fs::remove_file(&previous)?;
        return Ok(true);
    }
    let was_first = first.exists();
    if was_first {
        remove_if_there(&place.file(service))?;
        fs::remove_file(&first)?;
    }
    Ok(was_first)
}

/// Forgets the declaration kept for a restore: the update that installed the new one succeeded.
///
/// # Errors
/// A removal that failed.
pub fn settle(place: &Place<'_>) -> io::Result<()> {
    remove_if_there(&place.state.join(PREVIOUS))?;
    remove_if_there(&place.state.join(FIRST))
}

fn remove_if_there(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Writes `name` in `dir` with `mode`: a temporary file beside it, synced and renamed over it,
/// then the directory synced. The temporary name does not end in `.toml`, so a load in between
/// does not read it.
fn write_file(dir: &Path, name: &str, bytes: &[u8], mode: u32) -> io::Result<()> {
    let tmp = dir.join(format!(".{name}.new"));
    remove_if_there(&tmp)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)?;
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&tmp, dir.join(name))?;
    File::open(dir)?.sync_all()
}
