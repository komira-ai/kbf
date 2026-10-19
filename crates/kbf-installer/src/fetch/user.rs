//! The fetch user: `kbf-fetch` (`_kbf-fetch` on macOS), a system user with no home, no
//! shell and no groups that owns no file (contract section 3). The package or `setup`
//! creates it; this reads its ids.

use std::ffi::CString;
use std::io;

use super::FETCH_USER;
use crate::layout::Platform;
use crate::run::RunAs;

/// The name of the fetch user on `platform`.
pub const fn name(platform: Platform) -> &'static str {
    match platform {
        Platform::Linux => FETCH_USER,
        Platform::MacOs => "_kbf-fetch",
    }
}

/// Why the fetch user's ids could not be had.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum UserFault {
    #[error("there is no user {0:?}")]
    Missing(String),
    #[error("the user {0:?} is root: untrusted bytes are never handled as root")]
    Root(String),
    #[error("looking up {name:?}: {why}")]
    Lookup { name: String, why: String },
}

/// The uid and gid of the user called `name`, which must not be root.
pub fn lookup(name: &str) -> Result<RunAs, UserFault> {
    lookup_with(name, 16 * 1024)
}

/// [`lookup`] with a buffer of `buffer_len` bytes for the C library to fill (a name that
/// needs more is an error, `ERANGE`).
fn lookup_with(name: &str, buffer_len: usize) -> Result<RunAs, UserFault> {
    let fault = |why: String| UserFault::Lookup {
        name: name.to_owned(),
        why,
    };
    let c_name = CString::new(name).map_err(|e| fault(e.to_string()))?;
    // SAFETY: an all-zero passwd is valid storage for getpwnam_r to fill.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    let mut buffer = vec![0; buffer_len];
    // SAFETY: every pointer is to storage that lives through the call, and the buffer
    // length is the one given.
    let rc = unsafe {
        libc::getpwnam_r(
            c_name.as_ptr(),
            &raw mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &raw mut found,
        )
    };
    match answer(rc, !found.is_null()) {
        Answer::Found => {}
        Answer::NoSuchUser => return Err(UserFault::Missing(name.to_owned())),
        Answer::Error(errno) => return Err(fault(io::Error::from_raw_os_error(errno).to_string())),
    }
    if runs_as_root(entry.pw_uid, entry.pw_gid) {
        return Err(UserFault::Root(name.to_owned()));
    }
    Ok(RunAs {
        uid: entry.pw_uid,
        gid: entry.pw_gid,
    })
}

/// Whether an account is root or in root's group: either is a way to read what only root
/// should.
fn runs_as_root(uid: u32, gid: u32) -> bool {
    uid == 0 || gid == 0
}

/// What `getpwnam_r` said.
#[derive(Debug, PartialEq, Eq)]
enum Answer {
    Found,
    NoSuchUser,
    Error(i32),
}

/// POSIX leaves an absent user to be reported as a null result with 0 or one of a few
/// `errno`s, depending on the C library.
fn answer(rc: libc::c_int, found: bool) -> Answer {
    match (rc, found) {
        (0, true) => Answer::Found,
        (0 | libc::ENOENT | libc::ESRCH | libc::EBADF | libc::EPERM, _) => Answer::NoSuchUser,
        (errno, _) => Answer::Error(errno),
    }
}

#[cfg(test)]
#[path = "user_tests.rs"]
mod tests;
