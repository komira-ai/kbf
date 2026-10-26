//! Extended attributes of the files staged into the guest (`stage`).
//!
//! macOS keeps some of an app's code signature outside the file's bytes: in a file that is
//! not a Mach-O the signature lives in `com.apple.cs.CodeDirectory`,
//! `com.apple.cs.CodeRequirements`, `com.apple.cs.CodeSignature` and the like, and
//! `com.apple.ResourceFork` and `com.apple.FinderInfo` can matter to the app too. The
//! file-by-file copy therefore copies every attribute of a node it recreates, except the two
//! in [`SKIPPED`] ([`QUARANTINE`] and [`MACL`]); the tree clone carries them by itself and
//! has the same two removed afterwards. Both ways of building Xcode.app end with the same
//! attributes. (Measured on a farm Mac, an App Store Xcode carries, apart from these, only
//! `com.apple.appstore.*` attributes on the app root, which are copied.)
//!
//! `com.apple.quarantine` is left out because it marks a file as downloaded: the guest
//! would ask Gatekeeper about Xcode on first launch, and a headless guest cannot answer.
//! The operator installed this Xcode; the guest must not treat it as fetched.
//!
//! `com.apple.macl` is left out because it is a per-machine access list: macOS keeps it
//! SIP-protected, so not even root can write it, and the list of apps allowed to open the
//! file means nothing on another machine. Trying to copy it would fail the stage on the app
//! root. On the clone path its removal is attempted like the quarantine mark's; if macOS
//! refuses to remove it (EPERM or EACCES), that one refusal is ignored, because the guest
//! ignores a list that is not its own. Any other attribute that cannot be read or written
//! still fails the stage.
//!
//! A file's attributes are copied between the open files (the source was opened without
//! following a link and checked to be a regular file), so they are the attributes of the
//! file whose bytes were copied. A directory and a link are addressed by path with
//! `XATTR_NOFOLLOW`, which reads and writes the attributes of the link itself, never of
//! what it points at; a link is copied too because an attribute of a link (a resource
//! fork, a Finder tag) is part of the app as shipped and costs one call.
//!
//! An attribute list longer than [`MAX_NAMES_BYTES`] or a value longer than
//! [`MAX_VALUE_BYTES`] is refused (the stage fails naming the path): skipping it could
//! drop part of a signature and leave an Xcode that fails validation in the guest, which
//! is worse than no Xcode. Real attributes are far smaller (a resource fork is the
//! largest and stays below tens of MiB).
//!
//! Only macOS has this code. On other systems every function here does nothing: the
//! guest volume is a macOS volume, and the pure parts ([`name_list`], [`copied`],
//! [`check_len`]) are tested everywhere.

use std::ffi::CStr;
use std::fs::File;
use std::path::Path;

/// The attribute that marks a file as downloaded; never copied, removed from a clone.
pub(crate) const QUARANTINE: &str = "com.apple.quarantine";
/// The per-machine, SIP-protected access list; never copied, removal from a clone is tried
/// and a refusal is ignored.
pub(crate) const MACL: &str = "com.apple.macl";
/// The attributes that are not carried into the guest.
pub(crate) const SKIPPED: [&str; 2] = [QUARANTINE, MACL];
/// The longest attribute name list (all names, NUL-terminated) read from one node.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) const MAX_NAMES_BYTES: usize = 1 << 20;
/// The longest value of one attribute.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) const MAX_VALUE_BYTES: usize = 64 << 20;

/// The names in a list as the kernel returns it: each name followed by a NUL. An empty name
/// or text after the last NUL is an error.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn name_list(list: &[u8]) -> Result<Vec<&CStr>, String> {
    let mut names = Vec::new();
    let mut rest = list;
    while !rest.is_empty() {
        let name = CStr::from_bytes_until_nul(rest)
            .map_err(|_| "the attribute list does not end with a NUL".to_owned())?;
        if name.is_empty() {
            return Err("the attribute list holds an empty name".to_owned());
        }
        rest = &rest[name.to_bytes_with_nul().len()..];
        names.push(name);
    }
    Ok(names)
}

/// Whether the attribute `name` is copied: all but those in [`SKIPPED`].
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn copied(name: &CStr) -> bool {
    !SKIPPED.iter().any(|s| name.to_bytes() == s.as_bytes())
}

/// Whether a failed removal of the attribute `name` (errno `errno`) is ignored: only macOS
/// refusing to remove [`MACL`] (EPERM or EACCES, because SIP protects it). Anything else
/// fails the stage.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn removal_refusal_is_ignored(name: &str, errno: Option<i32>) -> bool {
    name == MACL && matches!(errno, Some(libc::EPERM | libc::EACCES))
}

/// Refuses a length of `what` above `max`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn check_len(what: &str, len: usize, max: usize) -> Result<(), String> {
    if len > max {
        Err(format!(
            "{what} is {len} bytes, more than the {max} allowed"
        ))
    } else {
        Ok(())
    }
}

/// Copies the attributes of the open file `src` (at `src_path`, for messages) to the open
/// file `dst`, all but those in [`SKIPPED`].
///
/// # Errors
/// An attribute could not be read or written, or is larger than the bounds allow.
#[cfg(target_os = "macos")]
pub(crate) fn copy_fd(src: &File, dst: &File, src_path: &Path) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    sys::copy(
        sys::At::Fd(src.as_raw_fd()),
        sys::At::Fd(dst.as_raw_fd()),
        MAX_VALUE_BYTES,
    )
    .map_err(|e| format!("extended attributes of {}: {e}", src_path.display()))
}

/// Copies the attributes of `src` (a directory or a link, itself, not followed) to `dst`.
///
/// # Errors
/// As [`copy_fd`].
#[cfg(target_os = "macos")]
pub(crate) fn copy_nofollow(src: &Path, dst: &Path) -> Result<(), String> {
    copy_nofollow_with(src, dst, MAX_VALUE_BYTES)
}

/// [`copy_nofollow`] with the value bound a test lowers.
#[cfg(target_os = "macos")]
pub(crate) fn copy_nofollow_with(src: &Path, dst: &Path, max_value: usize) -> Result<(), String> {
    let (s, d) = (sys::cstr(src)?, sys::cstr(dst)?);
    sys::copy(sys::At::Path(&s), sys::At::Path(&d), max_value)
        .map_err(|e| format!("extended attributes of {}: {e}", src.display()))
}

/// Removes the attributes in [`SKIPPED`] from `path` itself (a link is not followed); absent
/// is fine, and so is macOS refusing to remove [`MACL`] (see [`removal_refusal_is_ignored`]).
///
/// # Errors
/// An attribute exists and could not be removed, other than that refusal.
#[cfg(target_os = "macos")]
pub(crate) fn strip_skipped(path: &Path) -> Result<(), String> {
    let p = sys::cstr(path)?;
    for attr in SKIPPED {
        let name = sys::cstr(Path::new(attr))?;
        // SAFETY: both are NUL-terminated; `XATTR_NOFOLLOW` acts on a final link itself.
        let rc = unsafe { libc::removexattr(p.as_ptr(), name.as_ptr(), libc::XATTR_NOFOLLOW) };
        if rc == 0 {
            continue;
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ENOATTR)
            && !removal_refusal_is_ignored(attr, e.raw_os_error())
        {
            return Err(format!("remove {attr} from {}: {e}", path.display()));
        }
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn copy_fd(_src: &File, _dst: &File, _src_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn copy_nofollow(_src: &Path, _dst: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn strip_skipped(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "macos")]
mod sys {
    use std::ffi::{CStr, CString, c_char, c_void};
    use std::io;
    use std::os::fd::RawFd;
    use std::path::Path;

    use super::{MAX_NAMES_BYTES, check_len, copied, name_list};

    /// What the attributes belong to: an open file, or a path whose last component is not
    /// followed.
    #[derive(Clone, Copy)]
    pub(super) enum At<'a> {
        Fd(RawFd),
        Path(&'a CStr),
    }

    pub(super) fn cstr(p: &Path) -> Result<CString, String> {
        CString::new(p.as_os_str().as_encoded_bytes())
            .map_err(|_| format!("{} holds a NUL", p.display()))
    }

    fn len(rc: isize) -> io::Result<usize> {
        usize::try_from(rc).map_err(|_| io::Error::last_os_error())
    }

    /// The names of the attributes into `buf` (null and 0 asks for the size).
    fn list(at: At<'_>, buf: *mut c_char, size: usize) -> io::Result<usize> {
        // SAFETY: `buf` is null with `size` 0, or points at `size` writable bytes (the
        // callers pass a `Vec`'s); the path is NUL-terminated, the fd is open for the call.
        len(unsafe {
            match at {
                At::Fd(fd) => libc::flistxattr(fd, buf, size, 0),
                At::Path(p) => libc::listxattr(p.as_ptr(), buf, size, libc::XATTR_NOFOLLOW),
            }
        })
    }

    /// The value of `name` into `buf` (null and 0 asks for the size).
    fn get(at: At<'_>, name: &CStr, buf: *mut c_void, size: usize) -> io::Result<usize> {
        // SAFETY: as in `list`; `name` is NUL-terminated; position 0 reads from the start.
        len(unsafe {
            match at {
                At::Fd(fd) => libc::fgetxattr(fd, name.as_ptr(), buf, size, 0, 0),
                At::Path(p) => libc::getxattr(
                    p.as_ptr(),
                    name.as_ptr(),
                    buf,
                    size,
                    0,
                    libc::XATTR_NOFOLLOW,
                ),
            }
        })
    }

    fn set(at: At<'_>, name: &CStr, value: &[u8]) -> io::Result<()> {
        // SAFETY: `value` is `value.len()` readable bytes; the rest as in `get`.
        let rc = unsafe {
            match at {
                At::Fd(fd) => {
                    libc::fsetxattr(fd, name.as_ptr(), value.as_ptr().cast(), value.len(), 0, 0)
                }
                At::Path(p) => libc::setxattr(
                    p.as_ptr(),
                    name.as_ptr(),
                    value.as_ptr().cast(),
                    value.len(),
                    0,
                    libc::XATTR_NOFOLLOW,
                ),
            }
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// Copies every attribute of `from` but those in [`SKIPPED`] to `to`; a value above
    /// `max_value` is refused.
    pub(super) fn copy(from: At<'_>, to: At<'_>, max_value: usize) -> Result<(), String> {
        let total = list(from, std::ptr::null_mut(), 0).map_err(|e| format!("list: {e}"))?;
        if total == 0 {
            return Ok(());
        }
        check_len("the attribute name list", total, MAX_NAMES_BYTES)?;
        let mut names = vec![0u8; total];
        let got = list(from, names.as_mut_ptr().cast(), total)
            .map_err(|e| format!("list (the attributes changed while they were read?): {e}"))?;
        names.truncate(got);
        for name in name_list(&names)?.into_iter().filter(|n| copied(n)) {
            let shown = name.to_string_lossy();
            let size = get(from, name, std::ptr::null_mut(), 0)
                .map_err(|e| format!("read {shown}: {e}"))?;
            check_len(&format!("the attribute {shown}"), size, max_value)?;
            let mut value = vec![0u8; size];
            let got = get(from, name, value.as_mut_ptr().cast(), size)
                .map_err(|e| format!("read {shown}: {e}"))?;
            value.truncate(got);
            set(to, name, &value).map_err(|e| format!("write {shown}: {e}"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    /// Catches a list parser that drops a name, merges two, accepts an unterminated or
    /// empty name, or reads past the list.
    #[test]
    fn a_name_list_is_split_at_each_nul() {
        let names = name_list(b"com.apple.cs.CodeDirectory\0com.apple.FinderInfo\0").expect("list");
        let names: Vec<_> = names.iter().map(|n| n.to_str().expect("utf8")).collect();
        assert_eq!(
            names,
            ["com.apple.cs.CodeDirectory", "com.apple.FinderInfo"]
        );
        assert!(name_list(b"").expect("empty").is_empty());
        assert!(name_list(b"a\0b").is_err());
        assert!(name_list(b"a\0\0b\0").is_err());
    }

    /// Catches a quarantine mark or a macl list that is copied (the stage would then fail on
    /// the app root, where root cannot write macl), and a signature attribute, an App Store
    /// attribute or a lookalike that is dropped.
    #[test]
    fn only_the_quarantine_mark_and_the_macl_list_are_left_out() {
        let kept = |n: &str| copied(&CString::new(n).expect("name"));
        assert_eq!(SKIPPED, ["com.apple.quarantine", "com.apple.macl"]);
        assert!(!kept("com.apple.quarantine"));
        assert!(!kept("com.apple.macl"));
        for n in [
            "com.apple.appstore.metadata",
            "com.apple.appstore.store_cohort",
            "com.apple.appstore.storefront",
            "com.apple.appstore.vendor_name",
            "com.apple.macl2",
            "com.apple.mac",
            "com.apple.macL",
            "com.apple.cs.CodeDirectory",
            "com.apple.cs.CodeRequirements",
            "com.apple.cs.CodeSignature",
            "com.apple.ResourceFork",
            "com.apple.FinderInfo",
            "com.apple.quarantine2",
            "com.apple.quarantin",
        ] {
            assert!(kept(n), "{n}");
        }
    }

    /// Catches a refusal to remove macl that fails the stage, and the same leniency spreading
    /// to the quarantine mark, to another attribute or to another error (a failed removal of
    /// anything else must stay fatal).
    #[test]
    fn only_a_refused_removal_of_macl_is_ignored() {
        for e in [libc::EPERM, libc::EACCES] {
            assert!(removal_refusal_is_ignored(MACL, Some(e)));
            assert!(!removal_refusal_is_ignored(QUARANTINE, Some(e)));
            assert!(!removal_refusal_is_ignored(
                "com.apple.cs.CodeSignature",
                Some(e)
            ));
        }
        assert!(!removal_refusal_is_ignored(MACL, Some(libc::EIO)));
        assert!(!removal_refusal_is_ignored(MACL, None));
    }

    /// Catches a bound that is off by one, in either direction.
    #[test]
    fn a_length_above_the_bound_is_refused_and_one_at_it_is_not() {
        assert!(check_len("v", 10, 10).is_ok());
        let e = check_len("the attribute x", 11, 10).expect_err("over");
        assert_eq!(e, "the attribute x is 11 bytes, more than the 10 allowed");
    }

    /// Catches a non-macOS build that fails on a node because it cannot read attributes.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn off_macos_nothing_is_copied_and_nothing_fails() {
        let dir = crate::fake::tmp("xattr-off");
        let f = std::fs::File::create(dir.join("f")).expect("file");
        assert!(copy_fd(&f, &f, Path::new("f")).is_ok());
        assert!(copy_nofollow(&dir, &dir).is_ok());
        assert!(strip_skipped(&dir).is_ok());
    }
}
