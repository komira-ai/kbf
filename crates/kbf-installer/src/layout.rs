//! Where a host keeps kbf's releases and the installer's own state, in one place.
//!
//! The contract is `docs/design/kbf-installer.md`, section 3. On Linux:
//!
//! ```text
//! /var/lib/kbf-installer/                  root:root 0755; the installer's own tree
//!   releases/<commit>/                     one release: its binaries, units/, manifest.json
//!   releases/.stage-<commit>.<rand>/       a release being installed (renamed into place)
//!   releases/current                       link to <commit>, the release that runs
//!   releases/current.new, previous.new     a link being replaced (renamed over its name)
//!   releases/previous                      link to <commit>, the one before it
//!   state.json, state.json.bak             the state (crate::state) and the last good one
//!   work/                                  scratch of a stage in progress (crate::stage)
//! /run/kbf-installer/lock                  the lock the state is read and written under
//! /usr/share/kbf-installer/trust/          what offline verification trusts (package-owned)
//!   gh, trusted_root.jsonl, installer.conf
//! ```
//!
//! macOS differs in four paths ([`Platform`]): the installer tree is root's
//! `/usr/local/kbf-installer` (never a user-owned Homebrew prefix), the state is in
//! `/var/db/kbf-installer` (with `work/`), the lock is `/var/run/kbf-installer/lock`, and the
//! trust directory is `/usr/local/share/kbf-installer/trust`.
//!
//! Nothing else is named anywhere else: `kbf-deploy host ...`, the helper and the tests ask a
//! [`Layout`]. The kbf runtime's own state, `/var/lib/kbf`, is not here and is never touched.
//!
//! The paths are [`HostPath`]s, joined to a [`kbf_host::Root`] (`/` on a host, a scratch
//! directory in tests) inside a [`Layout`]. The links name their release relative to their own
//! directory (`<commit>`, beside them in `releases/`), so a tree copied or mounted elsewhere
//! still points into itself.

use std::path::PathBuf;

use kbf_host::path::{HostPath, Root};

use super::CommitId;

/// The installer's tree, and where its state is, per platform.
const LINUX_ROOT: &str = "/var/lib/kbf-installer";
const MACOS_ROOT: &str = "/usr/local/kbf-installer";
const MACOS_STATE_DIR: &str = "/var/db/kbf-installer";
const LINUX_LOCK: &str = "/run/kbf-installer/lock";
const MACOS_LOCK: &str = "/var/run/kbf-installer/lock";
const LINUX_TRUST: &str = "/usr/share/kbf-installer/trust";
const MACOS_TRUST: &str = "/usr/local/share/kbf-installer/trust";

/// The files of the trust directory.
pub const GH: &str = "gh";
pub const TRUSTED_ROOT: &str = "trusted_root.jsonl";
pub const SETTINGS: &str = "installer.conf";

/// The directory of releases, and the links in it, by name.
pub const RELEASES: &str = "releases";
pub const CURRENT: &str = "current";
pub const PREVIOUS: &str = "previous";
/// A link being replaced is made under its name and this suffix, then renamed over it.
pub const NEW_SUFFIX: &str = ".new";
/// A release being installed is made under this prefix in `releases/`, then renamed.
pub const STAGE_PREFIX: &str = ".stage-";
/// The state file's name, and the copy of the last good one.
pub const STATE_FILE: &str = "state.json";
pub const STATE_BACKUP: &str = "state.json.bak";

/// The names inside a release directory: its binaries, its systemd units and the
/// manifest that lists them. A Linux release holds `kbf`; a macOS release holds
/// `kbf-daemon` (the LaunchDaemon runs `current/kbf-daemon`).
pub const RELEASE_BINARY: &str = "kbf";
pub const RELEASE_DAEMON: &str = "kbf-daemon";
pub const RELEASE_UNITS: &str = "units";
pub const RELEASE_MANIFEST: &str = "manifest.json";
/// Every binary a release may hold, in the order they are copied and reported.
pub const RELEASE_BINARIES: [&str; 2] = [RELEASE_BINARY, RELEASE_DAEMON];

/// The operating system a host runs: what decides where its state and lock are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Linux,
    MacOs,
}

impl Platform {
    /// The platform this program was built for: macOS, else Linux.
    pub fn current() -> Platform {
        Platform::named(std::env::consts::OS)
    }

    /// The platform Rust calls `os` (`macos`, else Linux).
    pub fn named(os: &str) -> Platform {
        if os == "macos" {
            Platform::MacOs
        } else {
            Platform::Linux
        }
    }

    /// `linux` or `macos`, as the command line spells it.
    pub fn name(self) -> &'static str {
        match self {
            Platform::Linux => "linux",
            Platform::MacOs => "macos",
        }
    }

    fn root(self) -> &'static str {
        match self {
            Platform::Linux => LINUX_ROOT,
            Platform::MacOs => MACOS_ROOT,
        }
    }

    fn state_dir(self) -> &'static str {
        match self {
            Platform::Linux => LINUX_ROOT,
            Platform::MacOs => MACOS_STATE_DIR,
        }
    }

    fn lock(self) -> &'static str {
        match self {
            Platform::Linux => LINUX_LOCK,
            Platform::MacOs => MACOS_LOCK,
        }
    }

    fn trust(self) -> &'static str {
        match self {
            Platform::Linux => LINUX_TRUST,
            Platform::MacOs => MACOS_TRUST,
        }
    }
}

impl std::str::FromStr for Platform {
    type Err = String;

    fn from_str(name: &str) -> Result<Platform, String> {
        match name {
            "linux" => Ok(Platform::Linux),
            "macos" => Ok(Platform::MacOs),
            _ => Err(format!("{name:?} is not a platform (linux or macos)")),
        }
    }
}

/// A host's directory tree: the directory its paths are under and the platform that
/// decides the paths that differ. Everything that reads or changes a host's releases
/// takes one, so a host has one layout whoever asks.
#[derive(Clone, Debug)]
pub struct Layout {
    root: Root,
    platform: Platform,
}

impl Layout {
    pub fn new(root: Root, platform: Platform) -> Layout {
        Layout { root, platform }
    }

    /// A Linux host's layout under `root`.
    pub fn linux(root: Root) -> Layout {
        Layout::new(root, Platform::Linux)
    }

    pub fn root(&self) -> &Root {
        &self.root
    }

    pub fn platform(&self) -> Platform {
        self.platform
    }

    /// `path` as it is on this machine: under the root.
    pub fn join(&self, path: &HostPath) -> PathBuf {
        self.root.join(path)
    }

    /// The installer's tree: the releases and the links to them.
    pub fn install_root(&self) -> HostPath {
        path(self.platform.root())
    }

    /// `<install root>/releases`.
    pub fn releases(&self) -> HostPath {
        path_in(self.platform.root(), RELEASES)
    }

    /// `releases/current`: the release that runs.
    pub fn current(&self) -> HostPath {
        self.release_link_path(CURRENT)
    }

    /// `releases/previous`: the release before it.
    pub fn previous(&self) -> HostPath {
        self.release_link_path(PREVIOUS)
    }

    fn release_link_path(&self, name: &str) -> HostPath {
        path_in(&format!("{}/{RELEASES}", self.platform.root()), name)
    }

    /// `releases/<commit>`.
    pub fn release_dir(&self, commit: &CommitId) -> HostPath {
        path_in(
            &format!("{}/{RELEASES}", self.platform.root()),
            commit.as_str(),
        )
    }

    /// `releases/<commit>/<name>`, for a name of [`RELEASE_BINARY`], [`RELEASE_DAEMON`],
    /// [`RELEASE_UNITS`] or [`RELEASE_MANIFEST`].
    pub fn release_file(&self, commit: &CommitId, name: &'static str) -> HostPath {
        path_in(&self.release_dir(commit).to_string(), name)
    }

    /// `releases/.stage-<commit>.<rand>`: where a release is made before it is renamed to
    /// [`Layout::release_dir`]. `rand` is letters and digits.
    pub fn stage_dir(&self, commit: &CommitId, rand: &str) -> HostPath {
        path_in(
            &format!("{}/{RELEASES}", self.platform.root()),
            &format!("{STAGE_PREFIX}{commit}.{rand}"),
        )
    }

    /// The directory the state is in (the install root on Linux, `/var/db` on macOS).
    pub fn state_dir(&self) -> HostPath {
        path(self.platform.state_dir())
    }

    /// `state.json`.
    pub fn state_file(&self) -> HostPath {
        path_in(self.platform.state_dir(), STATE_FILE)
    }

    /// `state.json.bak`: the last state that read back whole.
    pub fn state_backup(&self) -> HostPath {
        path_in(self.platform.state_dir(), STATE_BACKUP)
    }

    /// The lock the state is read and written under.
    pub fn lock(&self) -> HostPath {
        path(self.platform.lock())
    }

    /// The directory a stage makes its scratch directory in (`work/`).
    pub fn work_dir(&self) -> HostPath {
        path_in(self.platform.state_dir(), "work")
    }

    /// The directory of what offline verification trusts.
    pub fn trust_dir(&self) -> HostPath {
        path(self.platform.trust())
    }

    /// The pinned `gh`.
    pub fn gh(&self) -> HostPath {
        path_in(self.platform.trust(), GH)
    }

    /// The Sigstore trusted root, as `gh attestation trusted-root` prints it.
    pub fn trusted_root(&self) -> HostPath {
        path_in(self.platform.trust(), TRUSTED_ROOT)
    }

    /// The host's settings (`installer.conf`), optional.
    pub fn settings(&self) -> HostPath {
        path_in(self.platform.trust(), SETTINGS)
    }
}

/// An absolute path as a [`HostPath`].
///
/// # Panics
///
/// When `path` is not a valid host path; every constant here is one (a test checks).
pub fn path(path: &str) -> HostPath {
    HostPath::new(path).expect("a layout constant is an absolute host path")
}

/// What `current` and `previous` point to for `commit`: `<commit>`, a sibling of the link.
pub fn release_link(commit: &CommitId) -> PathBuf {
    PathBuf::from(commit.as_str())
}

/// `dir/name`; `name` is a commit id or a constant, so it has no `/` or `..`.
fn path_in(dir: &str, name: &str) -> HostPath {
    HostPath::new(&format!("{dir}/{name}")).expect("a layout directory and a plain name")
}
