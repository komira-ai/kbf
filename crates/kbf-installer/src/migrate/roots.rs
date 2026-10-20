//! Where the old layout is, and where units are. The old layout is the deploy kit's
//! (`/usr/local/kbf`, `/var/lib/kbf-deploy`): nothing else names it any more, so it is named
//! here and only here. The installer's own tree and state are [`crate::layout::Layout`]'s;
//! the migration asks the layout for them like every other verb.

use kbf_host::path::HostPath;

use crate::layout::{self, Platform};

const OLD_TREE: &str = "/usr/local/kbf";
const OLD_STATE_LINUX: &str = "/var/lib/kbf-deploy";
const OLD_STATE_MACOS: &str = "/var/db/kbf-deploy";
const OLD_LOCK_LINUX: &str = "/run/kbf-deploy.lock";
const OLD_LOCK_MACOS: &str = "/var/run/kbf-deploy.lock";

/// Where the old layout is, and where unit files are.
#[derive(Clone, Debug)]
pub struct Roots {
    pub platform: Platform,
    /// `/usr/local/kbf`: `releases/`, `current`, `previous`.
    pub old_tree: HostPath,
    /// `/var/lib/kbf-deploy` (macOS `/var/db/kbf-deploy`): `state.json`, `helper.json`.
    pub old_state: HostPath,
    /// The lock the old tools changed their state under.
    pub old_lock: HostPath,
    /// Where unit files are: `/etc/systemd/system`, or `/Library/LaunchDaemons`.
    pub unit_dir: HostPath,
}

impl Roots {
    pub fn new(platform: Platform) -> Roots {
        let (state, lock, units) = match platform {
            Platform::Linux => (OLD_STATE_LINUX, OLD_LOCK_LINUX, "/etc/systemd/system"),
            Platform::MacOs => (OLD_STATE_MACOS, OLD_LOCK_MACOS, "/Library/LaunchDaemons"),
        };
        Roots {
            platform,
            old_tree: layout::path(OLD_TREE),
            old_state: layout::path(state),
            old_lock: layout::path(lock),
            unit_dir: layout::path(units),
        }
    }

    /// The binary a release's version is asked of.
    pub fn binary(&self) -> &'static str {
        match self.platform {
            Platform::Linux => layout::RELEASE_BINARY,
            Platform::MacOs => layout::RELEASE_DAEMON,
        }
    }
}
