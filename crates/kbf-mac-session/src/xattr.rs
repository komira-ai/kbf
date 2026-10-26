//! Extended attributes of an open directory: the calls the sweep needs to take off what a
//! lease set on a directory it keeps. Linux's are for the tests of the sweep on every
//! platform.

use std::io;
use std::os::fd::{AsRawFd as _, OwnedFd};

/// The names of the extended attributes of `fd`.
///
/// # Errors
/// The listing failed.
pub(crate) fn list(fd: &OwnedFd) -> io::Result<Vec<String>> {
    let raw = fd.as_raw_fd();
    let call = |buf: *mut libc::c_char, size: usize| -> isize {
        // SAFETY: `raw` is a descriptor `fd` keeps open; `buf` is null with size 0 (only
        // the size is asked for) or writable for `size` bytes.
        #[cfg(target_os = "macos")]
        let got = unsafe { libc::flistxattr(raw, buf, size, 0) };
        // SAFETY: as above.
        #[cfg(not(target_os = "macos"))]
        let got = unsafe { libc::flistxattr(raw, buf, size) };
        got
    };
    let size =
        usize::try_from(call(std::ptr::null_mut(), 0)).map_err(|_| io::Error::last_os_error())?;
    if size == 0 {
        return Ok(Vec::new());
    }
    let mut names = vec![0_u8; size];
    let got = usize::try_from(call(names.as_mut_ptr().cast(), size))
        .map_err(|_| io::Error::last_os_error())?;
    names.truncate(got);
    Ok(names
        .split(|b| *b == 0)
        .filter(|n| !n.is_empty())
        .map(|n| String::from_utf8_lossy(n).into_owned())
        .collect())
}

/// Removes the extended attribute `name` of `fd`.
///
/// # Errors
/// The removal failed.
pub(crate) fn remove(fd: &OwnedFd, name: &str) -> io::Result<()> {
    let name = std::ffi::CString::new(name).map_err(io::Error::other)?;
    // SAFETY: `fd` is an open descriptor; `name` is NUL-terminated.
    #[cfg(target_os = "macos")]
    let done = unsafe { libc::fremovexattr(fd.as_raw_fd(), name.as_ptr(), 0) };
    // SAFETY: as above.
    #[cfg(not(target_os = "macos"))]
    let done = unsafe { libc::fremovexattr(fd.as_raw_fd(), name.as_ptr()) };
    if done == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Whether `why`, from [`remove`], says the attribute is gone already or is the system's
/// own, which it protects from the owner.
#[cfg(target_os = "macos")]
pub(crate) fn left(why: &io::Error) -> bool {
    matches!(why.raw_os_error(), Some(libc::ENOATTR | libc::EPERM))
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn left(why: &io::Error) -> bool {
    matches!(why.raw_os_error(), Some(libc::ENODATA | libc::EPERM))
}
