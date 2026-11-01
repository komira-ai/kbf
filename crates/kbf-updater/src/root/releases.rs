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
//! nobody else can write. The tree is checked against `tree_digest` before every step
//! (`Staged::check`). `<releases>/<service>/current` is a link to the version the last update
//! that succeeded installed: the release a boot entry runs from, and the previous release of the
//! next update.

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use kbf_step_runner::Staged;
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// No such release, a seal that is missing, untrusted or not a seal, or a version that is
    /// not a file name.
    pub fn staged(&self, service: &str, version: &str) -> Result<Release, String> {
        if !is_version(version) {
            return Err(format!("{version:?} is not a version"));
        }
        let dir = self.service_dir(service);
        let seal_path = dir.join(format!("{version}{SEAL_SUFFIX}"));
        let bytes = update_rights::trust::read_trusted(&seal_path, self.trusted_uid)
            .map_err(|e| format!("the seal {}: {e}", seal_path.display()))?;
        let seal: Seal = serde_json::from_slice(&bytes)
            .map_err(|e| format!("the seal {}: {e}", seal_path.display()))?;
        Ok(Release {
            version: version.to_owned(),
            staged: Staged {
                dir: dir.join(version),
                digest: seal.tree_digest,
            },
            artifact_digest: seal.artifact_digest,
        })
    }

    /// The release the `current` link names, `None` when there is no link.
    ///
    /// # Errors
    /// A link that names no version, or a release that is not staged.
    pub fn current(&self, service: &str) -> Result<Option<Release>, String> {
        let link = self.service_dir(service).join(CURRENT);
        let target = match fs::read_link(&link) {
            Ok(target) => target,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", link.display())),
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
