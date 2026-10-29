use std::cell::RefCell;
use std::io;
use std::process::Command;

use super::{Creds, Libc, become_ids, switch_to};
use crate::runner::Identity;

/// Plays the calls and records them; the call named in `fail` fails.
struct Rec {
    calls: RefCell<Vec<String>>,
    fail: Option<&'static str>,
}

impl Rec {
    fn new(fail: Option<&'static str>) -> Self {
        Rec {
            calls: RefCell::new(Vec::new()),
            fail,
        }
    }

    fn call(&self, name: &'static str, text: String) -> io::Result<()> {
        self.calls.borrow_mut().push(text);
        if self.fail == Some(name) {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        Ok(())
    }
}

impl Creds for Rec {
    fn set_groups(&self, groups: &[u32]) -> io::Result<()> {
        self.call("groups", format!("groups {groups:?}"))
    }
    fn set_gid(&self, gid: u32) -> io::Result<()> {
        self.call("gid", format!("gid {gid}"))
    }
    fn set_uid(&self, uid: u32) -> io::Result<()> {
        self.call("uid", format!("uid {uid}"))
    }
}

const ID: Identity = Identity { uid: 501, gid: 20 };

/// Catches a drop in the wrong order (the groups cannot be set once the uid is not root's)
/// or with groups beyond the one gid: the groups are exactly `[gid]`, then the gid, then the uid.
#[test]
fn the_groups_are_the_one_gid_and_are_set_before_the_gid_and_the_uid() {
    let rec = Rec::new(None);
    become_ids(&rec, ID).unwrap();
    assert_eq!(*rec.calls.borrow(), ["groups [20]", "gid 20", "uid 501"]);
}

/// Catches a drop that goes on after a call failed (a child that kept root's uid or groups):
/// each failure ends it, and the calls after it are not made.
#[test]
fn a_failed_call_ends_the_drop() {
    for (fail, made) in [("groups", 1), ("gid", 2), ("uid", 3)] {
        let rec = Rec::new(Some(fail));
        let err = become_ids(&rec, ID).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(rec.calls.borrow().len(), made, "{fail}");
    }
}

/// Catches a drop that does not reach the system calls, or that reports a refusal as success:
/// the real calls of a caller who is not root. `setgroups` is refused, so a program started
/// with the drop never starts; a root caller (not the case in CI's unit tests) is not tested
/// here, `tests/real_drop.rs` does that.
#[test]
fn a_caller_who_is_not_root_cannot_start_a_dropped_program() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    let mut command = Command::new("/usr/bin/true");
    switch_to(&mut command, ID);
    assert!(command.spawn().is_err());
    assert!(Libc.set_groups(&[ID.gid]).is_err());
    // The caller's own ids are no change, so these are allowed.
    Libc.set_gid(rustix::process::getgid().as_raw()).unwrap();
    Libc.set_uid(rustix::process::getuid().as_raw()).unwrap();
}
