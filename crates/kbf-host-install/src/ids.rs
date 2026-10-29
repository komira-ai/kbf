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
        command.pre_exec(move || become_ids(id.uid, id.gid));
    }
}

/// `setgroups([gid])`, `setgid(gid)`, `setuid(uid)`, in that order (the groups cannot be set
/// once the uid is not root's). A call that fails fails the start of the program.
fn become_ids(uid: u32, gid: u32) -> io::Result<()> {
    let groups = [gid];
    // SAFETY: `groups` holds the one gid for the call to read; it does not keep the pointer.
    if unsafe { libc::setgroups(1, groups.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: setgid and setuid take any number; one the caller may not take fails.
    if unsafe { libc::setgid(gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::setuid(uid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
