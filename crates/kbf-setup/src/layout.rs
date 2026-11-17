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
//!   state.json, state.json.bak             the state (kbf_installer::state) and the last good one
//!   seq_floor                              the highest seq not allowed (a seed must exceed it)
//!   work/                                  scratch of a stage in progress (kbf_installer::stage)
//! /run/kbf-installer/lock                  the lock the state is read and written under
//! /usr/share/kbf/trust/                    what offline verification trusts (the kbf package's)
//!   gh, trusted_root.jsonl, installer.conf
//! /usr/share/kbf-installer/trust/          the same, where the retired kbf-installer package put it
//!                                          (read only when the directory above is absent: see
//!                                          [`Layout::with_trust_fallback`])
//! /usr/share/kbf-installer/seed/<commit12>/  the release the package carries (package-owned)
//!   manifest.json, kbf-<commit>-<platform>.tar.gz, and that file with .sigstore.json
//! ```
//!
//! The configuration directory is `/etc/kbf` (`kbf.toml`, `installer.toml`, the tokens). Only
//! Linux hosts run kbf, so [`Platform`] has the one value.
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

/// The installer's tree, and where its state is.
const LINUX_ROOT: &str = "/var/lib/kbf-installer";
const LINUX_LOCK: &str = "/run/kbf-installer/lock";
const LINUX_TRUST: &str = "/usr/share/kbf/trust";
/// Where the trust directory was while the `kbf-installer` package carried it. A host that has
/// not yet been given the `kbf` package that carries [`LINUX_TRUST`] still has its files here.
///
/// REMOVE IN v0.0.9: the release after the first one whose `kbf` package carries the trust
/// directory (v0.0.8). Delete this constant, [`Layout::with_trust_fallback`] and its callers.
const LINUX_TRUST_LEGACY: &str = "/usr/share/kbf-installer/trust";
/// What the package installs: the trust directory and the seed directory are in it.
const LINUX_PACKAGE: &str = "/usr/share/kbf-installer";
/// The configuration directory, and the files `setup` keeps in it.
const LINUX_CONFIG_DIR: &str = "/etc/kbf";
const LINUX_CONFIG: &str = "/etc/kbf/kbf.toml";
const LINUX_INSTALLER_TOML: &str = "/etc/kbf/installer.toml";
const LINUX_ENROLL_TOKEN: &str = "/etc/kbf/enroll.token";
const LINUX_API_TOKEN: &str = "/etc/kbf/api-token";

/// The directory of the packaged releases, one per build of the installer, in the package's
/// directory.
pub const SEED: &str = "seed";
/// The root-owned file beside `state.json` that holds the highest `seq` that is not allowed (a
/// staged release's `seq` must be strictly above it): decimal text, one number.
pub const SEQ_FLOOR: &str = "seq_floor";

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
/// The helper's memory (replay floors, the last event), beside the state.
pub const MEMORY: &str = "helper.json";
/// The order spool, in the state directory.
pub const ORDERS: &str = "orders";
/// The state file's name, and the copy of the last good one.
pub const STATE_FILE: &str = "state.json";
pub const STATE_BACKUP: &str = "state.json.bak";

/// The names inside a release directory: its binaries, its systemd units and the
/// manifest that lists them. A release holds `kbf`, its only binary.
pub const RELEASE_BINARY: &str = "kbf";
pub const RELEASE_UNITS: &str = "units";
pub const RELEASE_MANIFEST: &str = "manifest.json";
/// The marker written into a release directory once every check has passed. The verify
/// pipeline writes it with the commit. (The proxy once kept copies of its own that wrote it with
/// the sha256 of their binary; that supervision was removed.)
pub const VERIFIED: &str = "VERIFIED";

/// The operating system a host runs: what decides where its state and lock are. Only Linux
/// hosts run kbf.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Linux,
}

impl Platform {
    /// The platform this program runs kbf on.
    pub fn current() -> Platform {
        Platform::Linux
    }

    /// `linux`, as the command line spells it.
    pub fn name(self) -> &'static str {
        "linux"
    }

    fn root(self) -> &'static str {
        LINUX_ROOT
    }

    fn state_dir(self) -> &'static str {
        LINUX_ROOT
    }

    fn lock(self) -> &'static str {
        LINUX_LOCK
    }

    /// kbf's configuration directory.
    pub fn config_dir(self) -> &'static str {
        LINUX_CONFIG_DIR
    }

    /// The node's `kbf.toml`.
    pub fn kbf_toml(self) -> &'static str {
        LINUX_CONFIG
    }

    /// The installer's settings, `installer.toml`.
    pub fn installer_toml(self) -> &'static str {
        LINUX_INSTALLER_TOML
    }

    /// The enrollment token `setup` places.
    pub fn enroll_token(self) -> &'static str {
        LINUX_ENROLL_TOKEN
    }

    /// The first operator token.
    pub fn api_token(self) -> &'static str {
        LINUX_API_TOKEN
    }

    fn trust(self) -> &'static str {
        LINUX_TRUST
    }

    fn package(self) -> &'static str {
        LINUX_PACKAGE
    }
}

impl std::str::FromStr for Platform {
    type Err = String;

    fn from_str(name: &str) -> Result<Platform, String> {
        match name {
            "linux" => Ok(Platform::Linux),
            _ => Err(format!("{name:?} is not a platform (linux)")),
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
    /// The trust directory is the retired package's ([`LINUX_TRUST_LEGACY`]), not the `kbf`
    /// package's. Only [`Layout::with_trust_fallback`] sets it.
    trust_legacy: bool,
}

impl Layout {
    /// A layout whose trust directory is where the `kbf` package puts it.
    pub fn new(root: Root, platform: Platform) -> Layout {
        Layout {
            root,
            platform,
            trust_legacy: false,
        }
    }

    /// This layout, with the trust directory the retired `kbf-installer` package put on a
    /// Linux host if (and only if) the `kbf` package's directory is absent under the root and
    /// the old one is there, so a host that has not yet taken the new package keeps verifying.
    /// When both exist the new one is used, and so it is when neither does. Decided here, once:
    /// every path the layout then names agrees.
    ///
    /// REMOVE IN v0.0.9, with [`LINUX_TRUST_LEGACY`].
    #[must_use]
    pub fn with_trust_fallback(mut self) -> Layout {
        let is_dir = |dir: &str| self.root.join(&path(dir)).is_dir();
        self.trust_legacy = !is_dir(LINUX_TRUST) && is_dir(LINUX_TRUST_LEGACY);
        self
    }

    /// The trust directory this layout names.
    fn trust(&self) -> &'static str {
        if self.trust_legacy {
            LINUX_TRUST_LEGACY
        } else {
            self.platform.trust()
        }
    }

    /// A host's layout under `root`.
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

    /// `releases/<commit>/<name>`, for a name of [`RELEASE_BINARY`],
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

    /// The directory the state is in (the install root).
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

    /// The state directory, root's: where `status.json` is written.
    pub fn update_dir(&self) -> HostPath {
        self.state_dir()
    }

    /// The order spool, `orders/` in the state directory: owned by the user the node runs
    /// as (mode 0700, inside root's directory).
    pub fn orders_dir(&self) -> HostPath {
        path_in(self.platform.state_dir(), ORDERS)
    }

    /// The lock the state is read and written under.
    pub fn lock(&self) -> HostPath {
        path(self.platform.lock())
    }

    /// The directory a stage makes its scratch directory in (`work/`).
    pub fn work_dir(&self) -> HostPath {
        path_in(self.platform.state_dir(), "work")
    }

    /// The node's own `kbf.toml`: its `mode` says what this host runs.
    pub fn kbf_toml(&self) -> HostPath {
        path(self.platform.kbf_toml())
    }

    /// kbf's configuration directory.
    pub fn config_dir(&self) -> HostPath {
        path(self.platform.config_dir())
    }

    /// The installer's settings, `installer.toml`.
    pub fn installer_toml(&self) -> HostPath {
        path(self.platform.installer_toml())
    }

    /// The enrollment token `setup` places.
    pub fn enroll_token(&self) -> HostPath {
        path(self.platform.enroll_token())
    }

    /// The first operator token.
    pub fn api_token(&self) -> HostPath {
        path(self.platform.api_token())
    }

    /// The directory of what offline verification trusts.
    pub fn trust_dir(&self) -> HostPath {
        path(self.trust())
    }

    /// The pinned `gh`.
    pub fn gh(&self) -> HostPath {
        path_in(self.trust(), GH)
    }

    /// The Sigstore trusted root, as `gh attestation trusted-root` prints it.
    pub fn trusted_root(&self) -> HostPath {
        path_in(self.trust(), TRUSTED_ROOT)
    }

    /// The host's settings (`installer.conf`), optional.
    pub fn settings(&self) -> HostPath {
        path_in(self.trust(), SETTINGS)
    }

    /// `seed/<build>`, beside the trust directory: the release the package carries. `build`
    /// is the first 12 hex digits of the commit this installer was built from, a plain name
    /// (the caller checks that it is one).
    pub fn seed_dir(&self, build: &str) -> HostPath {
        path_in(&format!("{}/{SEED}", self.platform.package()), build)
    }

    /// A file of the seed directory of `build`, by its plain `name`.
    pub fn seed_file(&self, build: &str, name: &str) -> HostPath {
        path_in(self.seed_dir(build).as_str(), name)
    }

    /// `seq_floor`, beside the state: the highest `seq` that is not allowed (a staged
    /// release's `seq` must be strictly above it).
    pub fn seq_floor(&self) -> HostPath {
        path_in(self.platform.state_dir(), SEQ_FLOOR)
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

#[cfg(test)]
#[path = "layout_tests.rs"]
mod tests;
