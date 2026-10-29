//! The system calls of a privilege drop made between `fork` and `exec` of a child: the
//! supplementary groups set to exactly the one gid, then the gid, then the uid. This is the
//! only module of the crate that is not safe code.

#![allow(unsafe_code)]

use std::io;
use std::os::unix::process::CommandExt;
use std::process::Command;

use crate::runner::Identity;

/// Make `command` become `id` just before it executes its program, with the supplementary
/// groups `[id.gid]` and no others. The standard library's own `uid` and `gid` would run
/// before this and are not set on `command`.
pub(crate) fn switch_to(command: &mut Command, id: Identity) {
    // SAFETY: the closure runs in the child between fork and exec. It makes three system calls
    // on numbers copied in here, keeps no pointer past a call, and allocates nothing.
    unsafe {
        command.pre_exec(move || become_ids(&Libc, id));
    }
}

/// The three system calls that change who a process is, so a test plays them and reads the
/// order they are made in.
trait Creds {
    /// `setgroups`: the supplementary groups are exactly these.
    fn set_groups(&self, groups: &[u32]) -> io::Result<()>;
    fn set_gid(&self, gid: u32) -> io::Result<()>;
    fn set_uid(&self, uid: u32) -> io::Result<()>;
}

/// The groups, then the gid, then the uid (the groups cannot be set once the uid is not
/// root's). A call that fails ends it, and the program is not started.
fn become_ids(creds: &impl Creds, id: Identity) -> io::Result<()> {
    creds.set_groups(&[id.gid])?;
    creds.set_gid(id.gid)?;
    creds.set_uid(id.uid)
}

/// The real calls.
struct Libc;

fn check(result: libc::c_int) -> io::Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

impl Creds for Libc {
    fn set_groups(&self, groups: &[u32]) -> io::Result<()> {
        // SAFETY: `groups` holds the gids for the call to read; it does not keep the pointer.
        check(unsafe { libc::setgroups(groups.len() as _, groups.as_ptr()) })
    }

    fn set_gid(&self, gid: u32) -> io::Result<()> {
        // SAFETY: setgid takes any number; one the caller may not take fails.
        check(unsafe { libc::setgid(gid) })
    }

    fn set_uid(&self, uid: u32) -> io::Result<()> {
        // SAFETY: as above.
        check(unsafe { libc::setuid(uid) })
    }
}

#[cfg(test)]
mod tests;
