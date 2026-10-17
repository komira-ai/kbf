//! Helpers for tests of the host verbs, here and in the crates that wrap them (feature
//! `testkit`): three commits, a scratch root with the directories a host has above the
//! layout, and a state that sets every field.

use std::fs;
use std::path::PathBuf;

use kbf_host::path::Root;

use crate::CommitId;
use crate::state::{PendingConfirm, State};

pub const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
pub const B: &str = "0123456789abcdef0123456789abcdef01234567";
pub const C: &str = "cccccccccccccccccccccccccccccccccccccccc";

pub fn c(id: &str) -> CommitId {
    CommitId::new(id).expect("a test commit")
}

/// A scratch root with the directories a host has: `/run`, the state directory and
/// `/usr/local/kbf`.
pub fn host_root(name: &str) -> (Root, PathBuf) {
    let dir = kbf_host::testkit::scratch(name);
    for sub in ["run", "var/lib/kbf-deploy", "usr/local/kbf"] {
        fs::create_dir_all(dir.join(sub)).expect("seed");
    }
    (Root::new(&dir).expect("absolute"), dir)
}

pub fn full_state() -> State {
    State {
        current: Some(c(A)),
        previous: Some(c(B)),
        staged: Some(c(C)),
        pending_confirm: Some(PendingConfirm {
            commit: c(A),
            previous: Some(c(B)),
            deadline: 1_800_000_000,
        }),
        order_floor: 7,
    }
}
