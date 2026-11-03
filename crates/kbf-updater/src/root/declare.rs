//! The declaration a release carries, installed only through the gate
//! (docs/design/host-updates.md, 3.3).
//!
//! A verified release may carry `updater.d/<service>.toml`. Before `prepare` the root part reads
//! it from the verified tree and asks `update_rights::gate` about it against the active
//! declaration, read again from its trusted file at that moment: one the operator approved (by
//! digest), or one that asks for nothing more, is installed; anything else is `Deferred` with the
//! reason `rights_change` and nothing runs. With no active declaration, a first install is always
//! deferred until approved, whoever the hook runs as. A release that carries none, or the one that
//! is active, changes nothing.
//!
//! Installing first keeps, in the service's state directory, the active declaration (or the fact
//! that there was none) and the generation the step record had ([`BEGAN`]), so that an update
//! that then fails can put it back ([`restore`]), and a start after a crash can tell an install
//! whose update never began (the record still has that generation) from one whose update did. A
//! kept declaration is never overwritten: while one is kept, another install is refused.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use kbf_step_runner::{ArtifactError, Staged};
use update_rights::approval::ApprovalError;
use update_rights::declaration::Refusal;
use update_rights::{Approvals, Declaration, Declared, Deferral, Digest, Gate, gate};

/// The active declaration of an update in progress, kept to be put back.
pub const PREVIOUS: &str = "declaration.previous";
/// Kept instead when the update installed the first declaration of its service.
pub const FIRST: &str = "declaration.first";
/// The generation the step record had when the declaration was installed (`none` with no record).
pub const BEGAN: &str = "declaration.began";

/// Why a declaration a release carries was not decided on, or a kept one not put back.
#[derive(Debug, thiserror::Error)]
pub enum DeclareError {
    #[error("the declaration the release carries: {0}")]
    Artifact(#[source] ArtifactError),
    #[error("{}: {source}", .path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("the declaration is not UTF-8")]
    NotUtf8,
    #[error("the declaration: {0}")]
    Invalid(#[source] Refusal),
    #[error("the declaration names {found:?}, not {service:?}")]
    WrongName { found: String, service: String },
    #[error("the approvals: {0}")]
    Approvals(#[source] ApprovalError),
    #[error("the active declaration {} changed since it was loaded", .path.display())]
    ActiveChanged { path: PathBuf },
    #[error(
        "a declaration of {service} is kept for an update that has not ended; it is not replaced"
    )]
    Pending { service: String },
    #[error("{}: not a generation", .path.display())]
    BadMarker { path: PathBuf },
}

fn io_at(path: &Path) -> impl Fn(io::Error) -> DeclareError + '_ {
    move |source| DeclareError::Io {
        path: path.to_path_buf(),
        source,
    }
}

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
pub fn carried(staged: &Staged, service: &str) -> Result<Option<Vec<u8>>, DeclareError> {
    let inside = Path::new("updater.d").join(format!("{service}.toml"));
    match staged.check(&inside) {
        Ok(path) => fs::read(&path).map(Some).map_err(io_at(&path)),
        Err(ArtifactError::HookMissing(_)) => Ok(None),
        Err(e) => Err(DeclareError::Artifact(e)),
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

/// Decides on `bytes`, the declaration a release of `service` carries, against `active`, the
/// loaded declaration of the service if there is one, and installs it when the gate says so.
/// `generation` is that of the step record now (`None` with no record), kept with the active
/// declaration.
///
/// The active declaration's file is read again and must still be the one loaded: the comparison
/// is with the bytes the install replaces.
///
/// # Errors
/// A declaration that is not one, an approval file that cannot be trusted, an active file that
/// changed since it was loaded, a declaration already kept for an update that has not ended, or a
/// write that failed.
pub fn decide(
    place: &Place<'_>,
    active: Option<&Declared>,
    service: &str,
    bytes: &[u8],
    generation: Option<u64>,
) -> Result<Decision, DeclareError> {
    let digest = Digest::of(bytes);
    if active.map(Declared::digest) == Some(&digest) {
        return Ok(Decision::Unchanged);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| DeclareError::NotUtf8)?;
    let incoming = Declaration::parse(text).map_err(DeclareError::Invalid)?;
    if incoming.name != service {
        return Err(DeclareError::WrongName {
            found: incoming.name,
            service: service.to_owned(),
        });
    }
    let approvals =
        Approvals::read(place.approvals, place.trusted_uid).map_err(DeclareError::Approvals)?;
    let verdict = gate(
        active.map(Declared::declaration),
        &incoming,
        &digest,
        &approvals,
    );
    if let Gate::Deferred(deferral) = verdict {
        return Ok(Decision::Deferred(deferral));
    }
    if kept(place)? {
        return Err(DeclareError::Pending {
            service: service.to_owned(),
        });
    }
    let kept = match active {
        Some(declared) => {
            let file = declared.file();
            let now = fs::read(file).map_err(io_at(file))?;
            if &Digest::of(&now) != declared.digest() {
                return Err(DeclareError::ActiveChanged {
                    path: file.to_path_buf(),
                });
            }
            (PREVIOUS, now)
        }
        None => (FIRST, Vec::new()),
    };
    let began = generation.map_or_else(|| "none".to_owned(), |n| n.to_string());
    write_file(place.state, BEGAN, began.as_bytes(), 0o600)?;
    write_file(place.state, kept.0, &kept.1, 0o600)?;
    write_file(place.declarations, &format!("{service}.toml"), bytes, 0o644)?;
    Ok(Decision::Installed)
}

/// Whether a declaration is kept for an update that has not ended.
fn kept(place: &Place<'_>) -> Result<bool, DeclareError> {
    let mut kept = false;
    for path in [PREVIOUS, FIRST].map(|name| place.state.join(name)) {
        kept |= path.try_exists().map_err(io_at(&path))?;
    }
    Ok(kept)
}

/// The generation the step record had when a kept declaration was installed: `None` when none is
/// kept, `Some(None)` when there was no record then.
///
/// # Errors
/// The marker cannot be read, or holds no generation.
pub fn began(place: &Place<'_>) -> Result<Option<Option<u64>>, DeclareError> {
    let path = place.state.join(BEGAN);
    if !path.try_exists().map_err(io_at(&path))? {
        return Ok(None);
    }
    let text = fs::read_to_string(&path).map_err(io_at(&path))?;
    match text.trim() {
        "none" => Ok(Some(None)),
        number => number
            .parse()
            .map(|n| Some(Some(n)))
            .map_err(|_| DeclareError::BadMarker { path }),
    }
}

/// Puts back the declaration an update installed, if it installed one: the one that was active,
/// or none for a first install. `true` when it changed the directory of declarations. Only a kept
/// file that is not there counts as none kept; one that cannot be read is an error, and stays.
///
/// # Errors
/// A kept declaration that cannot be read, or a write that failed.
pub fn restore(place: &Place<'_>, service: &str) -> Result<bool, DeclareError> {
    let previous = place.state.join(PREVIOUS);
    let first = place.state.join(FIRST);
    let changed = match fs::read(&previous) {
        Ok(bytes) => {
            write_file(
                place.declarations,
                &format!("{service}.toml"),
                &bytes,
                0o644,
            )?;
            remove(&previous)?;
            true
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let was_first = first.try_exists().map_err(io_at(&first))?;
            if was_first {
                remove_if_there(&place.file(service))?;
                remove(&first)?;
            }
            was_first
        }
        Err(e) => return Err(io_at(&previous)(e)),
    };
    remove_if_there(&place.state.join(BEGAN))?;
    Ok(changed)
}

/// Forgets the declaration kept for a restore: the update that installed the new one succeeded.
///
/// # Errors
/// A removal that failed.
pub fn settle(place: &Place<'_>) -> Result<(), DeclareError> {
    for name in [PREVIOUS, FIRST, BEGAN] {
        remove_if_there(&place.state.join(name))?;
    }
    Ok(())
}

fn remove(path: &Path) -> Result<(), DeclareError> {
    fs::remove_file(path).map_err(io_at(path))
}

fn remove_if_there(path: &Path) -> Result<(), DeclareError> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(io_at(path)(e)),
        _ => Ok(()),
    }
}

/// Writes `name` in `dir` with `mode`: a temporary file beside it, made new without following a
/// link, synced and renamed over it, then the directory synced. The temporary name does not end
/// in `.toml`, so a load in between does not read it.
fn write_file(dir: &Path, name: &str, bytes: &[u8], mode: u32) -> Result<(), DeclareError> {
    let tmp = dir.join(format!(".{name}.new"));
    remove_if_there(&tmp)?;
    let target = dir.join(name);
    write_new(dir, &tmp, &target, bytes, mode).map_err(io_at(&target))
}

fn write_new(dir: &Path, tmp: &Path, target: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW)
        .open(tmp)?;
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(tmp, target)?;
    File::open(dir)?.sync_all()
}
