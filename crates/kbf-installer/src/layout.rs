//! Where a host keeps kbf's releases and kbf-deploy's own state and settings.
//!
//! ```text
//! /usr/local/kbf/releases/<commit>/   one release: its binaries, units/, manifest.json
//! /usr/local/kbf/current              link to releases/<commit>, the release that runs
//! /usr/local/kbf/previous             link to releases/<commit>, the one before it
//! <state dir>/incoming/               releases being received, not yet verified
//! <state dir>/staged/                 releases verified and ready to swap in
//! <state dir>/deployed/               a record of each release swapped in
//! <state dir>/state.json              the deploy state (crate::state)
//! <state dir>/work/                   scratch for a stage in progress (crate::stage)
//! /etc/kbf-deploy/policy.toml         what this host accepts
//! /etc/kbf-deploy/pins/               one <commit>.sha256 per pinned release
//! /etc/kbf-deploy/inventory.allow     the hosts a deploy may name
//! <lock>                              the lock the state is read and written under
//! /usr/local/libexec/kbf-deploy/      what offline verification trusts (below)
//! ```
//!
//! The two platforms differ in three paths ([`Platform`]):
//!
//! | | Linux | macOS |
//! |---|---|---|
//! | state dir | `/var/lib/kbf-deploy` | `/var/db/kbf-deploy` |
//! | lock | `/run/kbf-deploy.lock` | `/var/run/kbf-deploy.lock` |
//!
//! The Linux state dir is not renamed to match: live hosts use it. `/run` does not exist
//! on macOS, and `/var/lib` is not where macOS keeps system state.
//!
//! What offline verification trusts is one directory, `/usr/local/libexec/kbf-deploy/`,
//! root-owned and closed to group and other writes, on both platforms: the pinned `gh`
//! ([`GH`]), the Sigstore trusted root ([`TRUSTED_ROOT`]) and the host's settings
//! ([`SETTINGS`], optional). It is not under `/etc` because `/etc` is a symbolic link on
//! macOS and the trust check ([`crate::trust`]) refuses a link anywhere in a path. The
//! pins are installed by the host's bootstrap, not by this crate.
//!
//! The paths are [`HostPath`]s, joined to a [`kbf_host::Root`] (`/` on a host, a
//! scratch directory in tests) inside a [`Layout`]. The links name their release relative
//! to their own directory (`releases/<commit>`), so a tree copied or mounted elsewhere
//! still points into itself.

use std::path::{Path, PathBuf};

use kbf_host::path::{HostPath, Root};

use super::CommitId;

/// The directory the releases and the `current` and `previous` links are in.
pub const KBF_DIR: &str = "/usr/local/kbf";
pub const RELEASES: &str = "/usr/local/kbf/releases";
pub const CURRENT: &str = "/usr/local/kbf/current";
pub const PREVIOUS: &str = "/usr/local/kbf/previous";
/// The Linux state directory, and the paths in it (see [`Layout`] for a platform's).
pub const STATE_DIR: &str = "/var/lib/kbf-deploy";
pub const INCOMING: &str = "/var/lib/kbf-deploy/incoming";
pub const STAGED: &str = "/var/lib/kbf-deploy/staged";
pub const DEPLOYED: &str = "/var/lib/kbf-deploy/deployed";
pub const STATE_FILE: &str = "/var/lib/kbf-deploy/state.json";
pub const CONFIG_DIR: &str = "/etc/kbf-deploy";
pub const POLICY: &str = "/etc/kbf-deploy/policy.toml";
pub const PINS: &str = "/etc/kbf-deploy/pins";
pub const INVENTORY_ALLOW: &str = "/etc/kbf-deploy/inventory.allow";
/// The Linux lock.
pub const LOCK: &str = "/run/kbf-deploy.lock";
/// The macOS state directory and lock.
pub const MACOS_STATE_DIR: &str = "/var/db/kbf-deploy";
pub const MACOS_LOCK: &str = "/var/run/kbf-deploy.lock";
/// `/usr/local`, which a host may not have yet; an install makes it when it is missing.
pub const USR_LOCAL: &str = "/usr/local";

/// What offline verification trusts, on both platforms.
pub const INSTALLER_DIR: &str = "/usr/local/libexec/kbf-deploy";
pub const GH: &str = "/usr/local/libexec/kbf-deploy/gh";
pub const TRUSTED_ROOT: &str = "/usr/local/libexec/kbf-deploy/trusted_root.jsonl";
pub const SETTINGS: &str = "/usr/local/libexec/kbf-deploy/installer.conf";

/// The names inside a release directory: its binaries, its systemd units and the
/// manifest that lists them. Both platforms' releases hold `kbf`; it is the only binary
/// a macOS release holds (the LaunchDaemon runs `current/kbf daemon`). A Linux release
/// may also hold `kbf-daemon`, the separate daemon binary only Linux is built with.
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

    fn state_dir(self) -> &'static str {
        match self {
            Platform::Linux => STATE_DIR,
            Platform::MacOs => MACOS_STATE_DIR,
        }
    }

    fn lock(self) -> &'static str {
        match self {
            Platform::Linux => LOCK,
            Platform::MacOs => MACOS_LOCK,
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

    /// The directory the state, the stages in progress and the received releases are in.
    pub fn state_dir(&self) -> HostPath {
        path(self.platform.state_dir())
    }

    /// `state.json`.
    pub fn state_file(&self) -> HostPath {
        path_in(self.platform.state_dir(), "state.json")
    }

    /// The lock the state is read and written under.
    pub fn lock(&self) -> HostPath {
        path(self.platform.lock())
    }

    /// The directory a stage makes its scratch directory in.
    pub fn work_dir(&self) -> HostPath {
        path_in(self.platform.state_dir(), "work")
    }
}

/// One of the constants above as a [`HostPath`].
///
/// # Panics
///
/// When `path` is not a valid host path; every constant here is one (a test checks).
pub fn path(path: &'static str) -> HostPath {
    HostPath::new(path).expect("a layout constant is an absolute host path")
}

/// `/usr/local/kbf/releases/<commit>`.
pub fn release_dir(commit: &CommitId) -> HostPath {
    path_in(RELEASES, commit.as_str())
}

/// `/usr/local/kbf/releases/<commit>/<name>`, for a name of [`RELEASE_BINARY`],
/// [`RELEASE_DAEMON`], [`RELEASE_UNITS`] or [`RELEASE_MANIFEST`].
pub fn release_file(commit: &CommitId, name: &'static str) -> HostPath {
    path_in(&format!("{RELEASES}/{commit}"), name)
}

/// `/etc/kbf-deploy/pins/<commit>.sha256`.
pub fn pin_file(commit: &CommitId) -> HostPath {
    path_in(PINS, &format!("{commit}.sha256"))
}

/// What `current` and `previous` point to for `commit`: `releases/<commit>`.
pub fn release_link(commit: &CommitId) -> PathBuf {
    Path::new("releases").join(commit.as_str())
}

/// `dir/name`; `name` is a commit id or a constant, so it has no `/` or `..`.
fn path_in(dir: &str, name: &str) -> HostPath {
    HostPath::new(&format!("{dir}/{name}")).expect("a layout directory and a plain name")
}
