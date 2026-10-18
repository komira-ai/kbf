//! The directories the migration moves between. The old ones are the deploy kit's
//! ([`crate::layout`]); the new ones are the installer's (contract section 3), kept here,
//! in one place and small, until `layout.rs` carries them (contract P2) and this table is
//! replaced by a call to it.

use kbf_host::path::HostPath;

use crate::layout::{self, Platform};

/// Where the old layout is, where the new one goes, and where units are.
#[derive(Clone, Debug)]
pub struct Roots {
    pub platform: Platform,
    /// `/usr/local/kbf`: `releases/`, `current`, `previous`.
    pub old_tree: HostPath,
    /// `/var/lib/kbf-deploy` (macOS `/var/db/kbf-deploy`): `state.json`, `helper.json`.
    pub old_state: HostPath,
    /// The installer's tree: `releases/`, `work/`.
    pub new_tree: HostPath,
    /// The installer's state: `state.json`, `status.json`. The same directory as the tree
    /// on Linux (`/var/lib/kbf-installer`); on macOS the state is in `/var/db/kbf-installer`
    /// and the tree in `/usr/local/kbf-installer`.
    pub new_state: HostPath,
    /// Where unit files are: `/etc/systemd/system`, or `/Library/LaunchDaemons`.
    pub unit_dir: HostPath,
}

fn host(path: &str) -> HostPath {
    HostPath::new(path).expect("a constant host path")
}

impl Roots {
    pub fn new(platform: Platform) -> Roots {
        let (new_tree, new_state, unit_dir) = match platform {
            Platform::Linux => (
                "/var/lib/kbf-installer",
                "/var/lib/kbf-installer",
                "/etc/systemd/system",
            ),
            Platform::MacOs => (
                "/usr/local/kbf-installer",
                "/var/db/kbf-installer",
                "/Library/LaunchDaemons",
            ),
        };
        Roots {
            platform,
            old_tree: layout::path(layout::KBF_DIR),
            old_state: layout::path(match platform {
                Platform::Linux => layout::STATE_DIR,
                Platform::MacOs => layout::MACOS_STATE_DIR,
            }),
            new_tree: host(new_tree),
            new_state: host(new_state),
            unit_dir: host(unit_dir),
        }
    }

    /// The link a migrated unit runs the binaries from.
    pub fn new_bin(&self) -> String {
        format!("{}/releases/current", self.new_tree)
    }

    /// The binary a release's version is asked of.
    pub fn binary(&self) -> &'static str {
        match self.platform {
            Platform::Linux => layout::RELEASE_BINARY,
            Platform::MacOs => layout::RELEASE_DAEMON,
        }
    }
}
