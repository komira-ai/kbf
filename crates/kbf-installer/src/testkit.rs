//! Helpers for tests of the host verbs, here and in the crates that wrap them (feature
//! `testkit`): three commits, a scratch root with the directories a host has above the
//! layout, and a state that sets every field.

use std::fs;
use std::path::{Path, PathBuf};

use kbf_host::path::Root;

use crate::CommitId;
use crate::layout::{Layout, Platform};
use crate::state::{Floor, PendingConfirm, State, Unacked};

pub const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
pub const B: &str = "0123456789abcdef0123456789abcdef01234567";
pub const C: &str = "cccccccccccccccccccccccccccccccccccccccc";

pub fn c(id: &str) -> CommitId {
    CommitId::new(id).expect("a test commit")
}

/// A Linux host's layout on a scratch root with the directories a host has: `/run`, the
/// state directory and the releases directory; and the root's path.
pub fn host_root(name: &str) -> (Layout, PathBuf) {
    host_root_on(name, Platform::Linux)
}

/// The same for `platform`: its lock's directory (`/run`, or `/var/run` on macOS), its
/// state directory and the releases directory. A test that runs the installer binary uses the
/// platform it was built for, as the binary does.
pub fn host_root_on(name: &str, platform: Platform) -> (Layout, PathBuf) {
    let dir = kbf_host::testkit::scratch(name);
    let layout = Layout::new(Root::new(&dir).expect("absolute"), platform);
    let lock = layout.join(&layout.lock());
    let dirs = [
        lock.parent().expect("a lock has a directory").to_owned(),
        layout.join(&layout.state_dir()),
        layout.join(&layout.releases()),
    ];
    for sub in dirs {
        mkdirs(&dir, &sub, 0o755);
    }
    (layout, dir)
}

/// Create `path` and every missing directory between it and `root` (which must exist) with
/// exactly `mode`: the mode is set after the creation, so it does not depend on the process's
/// umask (a box with umask 0002 would otherwise lay down group-writable directories, which the
/// installer refuses in a state directory). Directories that already exist are left as they are.
pub fn mkdirs(root: &Path, path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let rel = path.strip_prefix(root).expect("a path under the root");
    let mut at = root.to_owned();
    for part in rel.components() {
        at.push(part);
        if !at.exists() {
            fs::create_dir(&at).expect("make the directory");
            fs::set_permissions(&at, fs::Permissions::from_mode(mode)).expect("set its mode");
        }
    }
}

pub fn full_state() -> State {
    State {
        current: Some(c(A)),
        previous: Some(c(B)),
        staged: Some(c(C)),
        pending_confirm: Some(PendingConfirm {
            activation: "3-9".to_owned(),
            commit: c(A),
            previous: Some(c(B)),
            deadline_wall: Some(1_800_000_000),
            deadline_mono_s: Some(86_400),
            starts_left: Some(2),
        }),
        floor: Floor {
            epoch: 3,
            order_id: 9,
            generation: 7,
        },
        serving_instance: Some("a".to_owned()),
        last_acked_order_id: 9,
        unacked: vec![Unacked {
            epoch: 3,
            order_id: 10,
            first_seen: 1_700_000_000,
            record: "{}".to_owned(),
        }],
    }
}
