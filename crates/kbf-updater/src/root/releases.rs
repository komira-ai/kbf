//! The staged releases the root part runs hooks from (docs/design/host-updates.md, 3.1 and
//! 3.4.1): `<releases>/<service>/<version>/` is the verified tree of one release, and
//! `<releases>/<service>/<version>.seal` beside it says what was verified:
//!
//! ```json
//! {"tree_digest": "<sha256 of the tree, kbf_step_runner::tree_digest>",
//!  "artifact_digest": "<sha256 of the attested tarball it was unpacked from>"}
//! ```
//!
//! The seal is written by the step that fetched and verified the release, right after the
//! verification; the root part reads it only if it is a regular file the trusted account owns and
//! nobody else can write. The tree is sealed and compared with `tree_digest` whenever the release
//! is looked up ([`sealed_as`]), and checked against it again before every step (`Staged::check`). `<releases>/<service>/current` is a link to the version the last update
//! that succeeded installed: the release a boot entry runs from, and the previous release of the
//! next update.

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use kbf_step_runner::{ArtifactError, Staged};
use serde::Deserialize;

/// The link to the release of the last update that succeeded.
pub const CURRENT: &str = "current";
/// The suffix of a seal file.
pub const SEAL_SUFFIX: &str = ".seal";

/// The longest version the root part takes.
pub const MAX_VERSION: usize = 128;

/// Whether `version` can name a release: 1 to 128 ASCII letters, digits, `.`, `-`, `_` and
/// `+`, not starting with `.`, and not the name of the `current` link. It is a file name, so it
/// can lead nowhere else.
pub fn is_version(version: &str) -> bool {
    let plain = version
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() | matches!(b, b'.' | b'-' | b'_' | b'+'));
    (1..=MAX_VERSION).contains(&version.len())
        & plain
        & !version.starts_with('.')
        & (version != CURRENT)
}

/// A field a later version adds to a seal is passed over; one of these missing is not a seal.
#[derive(Deserialize)]
struct Seal {
    tree_digest: String,
    artifact_digest: String,
}

/// One staged release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    /// The tree and the digest it was verified with.
    pub staged: Staged,
    /// The sha256 of the attested tarball, 64 lower-case hex digits.
    pub artifact_digest: String,
}

/// Why a staged release cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    #[error("{0:?} is not a version")]
    NotAVersion(String),
    #[error("the seal {}: {source}", .path.display())]
    SealUntrusted {
        path: PathBuf,
        #[source]
        source: update_rights::trust::TrustError,
    },
    #[error("the seal {}: {source}", .path.display())]
    NotASeal {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(transparent)]
    Tree(#[from] SealError),
    #[error("{}: {source}", .path.display())]
    Link {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Why a staged tree cannot be taken as the one that was verified.
#[derive(Debug, thiserror::Error)]
pub enum SealError {
    /// The tree cannot be read now (an I/O error, out of descriptors): that says nothing about
    /// the tree, and a later look may succeed.
    #[error("{0}")]
    Unreadable(String),
    /// The tree is not the one that was verified: it changed, or it is no longer in a trusted
    /// place. Nothing runs from it.
    #[error("{0}")]
    Changed(String),
}

/// The tree at `tree` as a [`Staged`], if it still has `digest`, the digest it was verified with:
/// a `Staged` is made only by sealing the tree as it is now, so the tree is sealed and the two
/// digests compared.
///
/// # Errors
/// [`SealError::Unreadable`] when the tree cannot be read now; [`SealError::Changed`] when it is
/// not the tree that was verified.
pub fn sealed_as(tree: &Path, digest: &str) -> Result<Staged, SealError> {
    let staged = Staged::seal(tree).map_err(|e| {
        let why = format!("the release {}: {e}", tree.display());
        match e {
            ArtifactError::Io(_) => SealError::Unreadable(why),
            _ => SealError::Changed(why),
        }
    })?;
    if staged.digest() != digest {
        return Err(SealError::Changed(format!(
            "the release {} changed after it was verified: it was {digest}, it is {}",
            tree.display(),
            staged.digest()
        )));
    }
    Ok(staged)
}

/// The directory of staged releases.
#[derive(Clone, Debug)]
pub struct Releases {
    dir: PathBuf,
    trusted_uid: u32,
}

impl Releases {
    pub fn new(dir: &Path, trusted_uid: u32) -> Releases {
        Releases {
            dir: dir.to_path_buf(),
            trusted_uid,
        }
    }

    fn service_dir(&self, service: &str) -> PathBuf {
        self.dir.join(service)
    }

    /// The release `version` of `service`, as it was sealed.
    ///
    /// # Errors
    /// No such release, a seal that is missing, untrusted or not a seal, a tree that is not the
    /// one sealed, or a version that is not a file name.
    pub fn staged(&self, service: &str, version: &str) -> Result<Release, ReleaseError> {
        if !is_version(version) {
            return Err(ReleaseError::NotAVersion(version.to_owned()));
        }
        let dir = self.service_dir(service);
        let seal_path = dir.join(format!("{version}{SEAL_SUFFIX}"));
        let bytes =
            update_rights::trust::read_trusted(&seal_path, self.trusted_uid).map_err(|source| {
                ReleaseError::SealUntrusted {
                    path: seal_path.clone(),
                    source,
                }
            })?;
        let seal: Seal =
            serde_json::from_slice(&bytes).map_err(|source| ReleaseError::NotASeal {
                path: seal_path.clone(),
                source,
            })?;
        let staged = sealed_as(&dir.join(version), &seal.tree_digest)?;
        Ok(Release {
            version: version.to_owned(),
            staged,
            artifact_digest: seal.artifact_digest,
        })
    }

    /// The release the `current` link names, `None` when there is no link.
    ///
    /// # Errors
    /// A link that cannot be read or names no version, or a release that is not staged.
    pub fn current(&self, service: &str) -> Result<Option<Release>, ReleaseError> {
        let link = self.service_dir(service).join(CURRENT);
        let target = match fs::read_link(&link) {
            Ok(target) => target,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(ReleaseError::Link { path: link, source }),
        };
        let version = target.to_str().unwrap_or_default();
        self.staged(service, version).map(Some)
    }

    /// Points `current` at `version`: a link made beside the old one and renamed over it, then the
    /// directory synced, so a crash leaves either link whole.
    ///
    /// # Errors
    /// What the file system says.
    pub fn set_current(&self, service: &str, version: &str) -> io::Result<()> {
        let dir = self.service_dir(service);
        let tmp = dir.join(format!(".{CURRENT}.tmp"));
        match fs::remove_file(&tmp) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        symlink(version, &tmp)?;
        fs::rename(&tmp, dir.join(CURRENT))?;
        File::open(&dir)?.sync_all()
    }
}
