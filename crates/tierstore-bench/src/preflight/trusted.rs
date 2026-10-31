//! The rule for a file the tool trusts as input and a person or another host's writer keeps:
//! the cold store's heal status file.

use std::fs::File;
use std::io::{self, Read as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::Path;

use rustix::fs::OFlags;
use rustix::io::Errno;
use rustix::process::geteuid;

/// The largest status file read, in bytes.
pub const STATUS_FILE_MAX: u64 = 64 * 1024;

/// Why a file with this metadata is not trusted, or `Ok`: it must be a regular file, owned by
/// `me`, not writable by group or others, and at most [`STATUS_FILE_MAX`] bytes.
///
/// # Errors
/// The reason, as text.
pub fn rule(is_file: bool, uid: u32, mode: u32, len: u64, me: u32) -> Result<(), &'static str> {
    if !is_file {
        return Err("it is not a regular file");
    }
    if uid != me {
        return Err("it is not owned by the user running the tool");
    }
    if mode & 0o022 != 0 {
        return Err("it is writable by its group or by others");
    }
    if len > STATUS_FILE_MAX {
        return Err("it is over the size cap of 64 KiB");
    }
    Ok(())
}

/// The bytes of the file at `path` if it passes [`rule`]. The last component is opened without
/// following a symbolic link and without waiting for a writer (so a FIFO does not hang the open),
/// and the checks are made on the open file, so what is checked is what is read.
///
/// # Errors
/// The operating system's error, or [`io::ErrorKind::PermissionDenied`] naming the fault.
pub fn read_trusted(path: &Path) -> io::Result<Vec<u8>> {
    let flags = (OFlags::NOFOLLOW | OFlags::NONBLOCK).bits();
    let file = File::options()
        .read(true)
        .custom_flags(i32::try_from(flags).unwrap_or(0))
        .open(path)
        .map_err(|e| {
            if e.raw_os_error() == Some(Errno::LOOP.raw_os_error()) {
                io::Error::new(io::ErrorKind::PermissionDenied, "it is a symbolic link")
            } else {
                e
            }
        })?;
    let meta = file.metadata()?;
    rule(
        meta.is_file(),
        meta.uid(),
        meta.mode(),
        meta.len(),
        geteuid().as_raw(),
    )
    .map_err(|why| io::Error::new(io::ErrorKind::PermissionDenied, why))?;
    let mut bytes = Vec::new();
    file.take(STATUS_FILE_MAX + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > STATUS_FILE_MAX {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "it is over the size cap of 64 KiB",
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches each part of the rule being dropped: a non-regular file, another owner, a mode
    /// bit of group or others that writes, and a file over the cap are each refused; the
    /// permissive read bits (0644, 0600, 0444) and a file of exactly the cap pass.
    #[test]
    fn the_rule_refuses_each_fault_and_only_those() {
        let me = 1000;
        assert_eq!(rule(true, me, 0o644, 10, me), Ok(()));
        assert_eq!(rule(true, me, 0o600, 10, me), Ok(()));
        assert_eq!(rule(true, me, 0o444, STATUS_FILE_MAX, me), Ok(()));
        assert!(rule(false, me, 0o644, 10, me).is_err());
        assert!(rule(true, me + 1, 0o644, 10, me).is_err());
        assert!(
            rule(true, 0, 0o644, 10, me).is_err(),
            "root's file is not mine"
        );
        for mode in [0o664, 0o646, 0o666, 0o620, 0o602] {
            assert!(rule(true, me, mode, 10, me).is_err(), "{mode:o}");
        }
        assert!(rule(true, me, 0o644, STATUS_FILE_MAX + 1, me).is_err());
    }
}
