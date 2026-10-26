//! Host file helpers the real system uses and unit tests can run on any Unix: names of
//! the files kept beside a bundle, the tail of a log, and an ownership change that cannot
//! be steered through a symbolic link.

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};

/// `<bundle's directory>/<bundle's name>.<suffix>`: a file kept beside a bundle. Two
/// bundles whose names differ only after a dot (`base.v1`, `base.v2`) get different files.
#[must_use]
pub fn sibling(bundle: &Path, suffix: &str) -> PathBuf {
    let name = bundle
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    bundle.with_file_name(format!("{name}.{suffix}"))
}

/// The last `max` bytes of the file at `path` as text; `None` when it cannot be read.
#[must_use]
pub fn tail_of(path: &Path, max: usize) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let start = bytes.len().saturating_sub(max);
    Some(String::from_utf8_lossy(&bytes[start..]).into_owned())
}

/// A libc return value as a result: negative is the failure `errno` names.
fn cvt(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

fn cstring(name: &Path) -> io::Result<CString> {
    CString::new(name.as_os_str().as_encoded_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
}

/// The entry names of the directory open as `dir`.
fn entry_names(dir: RawFd) -> io::Result<Vec<CString>> {
    // SAFETY: `dup` returns a new descriptor or -1.
    let copy = cvt(unsafe { libc::dup(dir) })?;
    // SAFETY: `copy` is a descriptor this function owns; on success `fdopendir` takes it.
    let stream = unsafe { libc::fdopendir(copy) };
    if stream.is_null() {
        let e = io::Error::last_os_error();
        // SAFETY: `fdopendir` failed, so `copy` is still ours to close.
        unsafe { libc::close(copy) };
        return Err(e);
    }
    let mut names = Vec::new();
    loop {
        // SAFETY: `stream` is the open directory stream from above.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        // SAFETY: `d_name` of a returned entry is a NUL-terminated string that lives until
        // the next `readdir`; it is copied before then.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_owned();
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            names.push(name);
        }
    }
    // SAFETY: `stream` is open and closed once; this also closes `copy`.
    unsafe { libc::closedir(stream) };
    Ok(names)
}

/// Gives `uid` and `gid` the directory open as `dir` and everything below it.
/// Every step is relative to a descriptor and none follows a symbolic link, so a link
/// planted in the tree while this runs is itself chowned, never what it points at.
fn walk(dir: RawFd, uid: u32, gid: u32) -> io::Result<()> {
    // SAFETY: `dir` is an open descriptor.
    cvt(unsafe { libc::fchown(dir, uid, gid) })?;
    for name in entry_names(dir)? {
        let mut st = std::mem::MaybeUninit::<libc::stat>::zeroed();
        // SAFETY: `name` is NUL-terminated and `st` is a stat buffer.
        cvt(unsafe {
            libc::fstatat(
                dir,
                name.as_ptr(),
                st.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        })?;
        // SAFETY: `fstatat` succeeded, so `st` is initialised.
        let is_dir = unsafe { st.assume_init() }.st_mode & libc::S_IFMT == libc::S_IFDIR;
        if is_dir {
            // SAFETY: `name` is NUL-terminated; `O_NOFOLLOW` refuses a link swapped in
            // since the stat above.
            let fd = cvt(unsafe {
                libc::openat(
                    dir,
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                )
            })?;
            // SAFETY: `fd` was just opened and nothing else owns it.
            let owned = unsafe { OwnedFd::from_raw_fd(fd) };
            walk(owned.as_raw_fd(), uid, gid)?;
        } else {
            // SAFETY: `name` is NUL-terminated; the flag makes this chown a link itself.
            cvt(unsafe {
                libc::fchownat(dir, name.as_ptr(), uid, gid, libc::AT_SYMLINK_NOFOLLOW)
            })?;
        }
    }
    Ok(())
}

/// Gives `uid` and `gid` ownership of `path` and of everything under it. `path` itself must not be
/// a symbolic link.
///
/// # Errors
/// A step failed (not root, or `path` is a link or does not exist); the message names `path`.
pub fn chown_tree(path: &Path, uid: u32, gid: u32) -> Result<(), String> {
    let describe = |e: io::Error| format!("chown {}: {e}", path.display());
    let c = cstring(path).map_err(describe)?;
    // SAFETY: `c` is a NUL-terminated path; `O_NOFOLLOW` refuses a symbolic link.
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(describe(io::Error::last_os_error()));
    }
    // SAFETY: `fd` was just opened and nothing else owns it.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut st = std::mem::MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: `owned` is open and `st` is a stat buffer.
    cvt(unsafe { libc::fstat(owned.as_raw_fd(), st.as_mut_ptr()) }).map_err(describe)?;
    // SAFETY: `fstat` succeeded, so `st` is initialised.
    let is_dir = unsafe { st.assume_init() }.st_mode & libc::S_IFMT == libc::S_IFDIR;
    if is_dir {
        walk(owned.as_raw_fd(), uid, gid).map_err(describe)
    } else {
        // SAFETY: `owned` is open.
        cvt(unsafe { libc::fchown(owned.as_raw_fd(), uid, gid) })
            .map(|_| ())
            .map_err(describe)
    }
}

/// The bytes an unprivileged writer can still write on the filesystem holding `path`
/// (`statvfs`: available blocks times the fragment size).
///
/// # Errors
/// `statvfs` failed.
pub fn free_bytes(path: &Path) -> io::Result<u64> {
    let c = cstring(path)?;
    let mut st = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: `c` is a NUL-terminated path and `st` is a statvfs buffer.
    cvt(unsafe { libc::statvfs(c.as_ptr(), st.as_mut_ptr()) })?;
    // SAFETY: `statvfs` succeeded, so `st` is initialised.
    let st = unsafe { st.assume_init() };
    // The field widths differ by platform; both are counts that fit in 64 bits.
    #[allow(clippy::useless_conversion, clippy::unnecessary_cast)]
    Ok((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
}

/// Makes `path` owned by uid 0 and gid 0. A symbolic link is changed itself, never what
/// it points at.
///
/// # Errors
/// `lchown` failed (not root, or the path does not exist).
pub fn lchown_root(path: &Path) -> io::Result<()> {
    let c = cstring(path)?;
    // SAFETY: `c` is a NUL-terminated path; `lchown` does not follow a final link.
    cvt(unsafe { libc::lchown(c.as_ptr(), 0, 0) }).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::tmp;
    use std::os::unix::fs::MetadataExt;

    fn me() -> u32 {
        // SAFETY: getuid cannot fail.
        unsafe { libc::getuid() }
    }

    fn my_group() -> u32 {
        // SAFETY: getgid cannot fail.
        unsafe { libc::getgid() }
    }

    /// Catches two bundles whose names differ after a dot sharing one spec path.
    #[test]
    fn a_sibling_keeps_the_whole_bundle_name() {
        let a = sibling(Path::new("/w/base.v1"), "spec.json");
        let b = sibling(Path::new("/w/base.v2"), "spec.json");
        assert_eq!(a, Path::new("/w/base.v1.spec.json"));
        assert_ne!(a, b);
        assert_eq!(sibling(Path::new("/"), "x"), Path::new("/.x"));
    }

    /// Catches a tail that keeps the head, the whole file, or fails on a short file.
    #[test]
    fn the_tail_is_the_last_bytes() {
        let dir = tmp("hostfs-tail");
        let f = dir.join("log");
        std::fs::write(&f, "0123456789").expect("log");
        assert_eq!(tail_of(&f, 4).as_deref(), Some("6789"));
        assert_eq!(tail_of(&f, 100).as_deref(), Some("0123456789"));
        assert!(tail_of(&dir.join("none"), 4).is_none());
    }

    /// Catches a walk that skips nested entries, follows a link out of the tree, or
    /// accepts a link as the root. The owner is the caller (a change to anyone else
    /// needs root), so the proof that a link is not followed is that its target keeps
    /// the owner it had and the walk finishes.
    #[test]
    fn the_tree_is_walked_without_following_links() {
        let dir = tmp("hostfs-chown");
        let root = dir.join("t");
        std::fs::create_dir_all(root.join("a/b")).expect("dirs");
        std::fs::write(root.join("a/b/f"), "x").expect("f");
        std::os::unix::fs::symlink(dir.join("outside"), root.join("a/link")).expect("dangling");
        std::os::unix::fs::symlink(&root, root.join("loop")).expect("loop");
        chown_tree(&root, me(), my_group()).expect("walk");
        assert_eq!(
            std::fs::metadata(root.join("a/b/f")).expect("f").uid(),
            me()
        );
        // A file as the root is chowned alone.
        chown_tree(&root.join("a/b/f"), me(), my_group()).expect("file");
        // A link as the root, and a path that is not there, are refused.
        let link = dir.join("rootlink");
        std::os::unix::fs::symlink(&root, &link).expect("link");
        assert!(
            chown_tree(&link, me(), my_group())
                .unwrap_err()
                .contains("chown")
        );
        assert!(chown_tree(&dir.join("none"), me(), my_group()).is_err());
        assert!(chown_tree(Path::new("a\0b"), me(), my_group()).is_err());
    }

    /// Catches a failure of the chown itself being swallowed (a user who is not root
    /// cannot give files away).
    #[test]
    fn a_refused_chown_is_an_error() {
        let dir = tmp("hostfs-chown-refused");
        let root = dir.join("t");
        std::fs::create_dir_all(root.join("a")).expect("dirs");
        std::fs::write(root.join("f"), "x").expect("f");
        // Only root may give files to someone else.
        let refused = me() != 0;
        assert_eq!(chown_tree(&root, me() + 1, my_group()).is_err(), refused);
        assert_eq!(
            chown_tree(&root.join("f"), me() + 1, my_group()).is_err(),
            refused
        );
    }

    /// Catches descriptor and directory-stream failures being ignored: a descriptor that
    /// is not open cannot be duplicated, and a file is not a directory.
    #[test]
    fn a_bad_descriptor_or_a_file_is_not_listed() {
        assert!(entry_names(-1).is_err());
        let dir = tmp("hostfs-names");
        std::fs::write(dir.join("f"), "x").expect("f");
        std::fs::create_dir(dir.join("d")).expect("d");
        let file = std::fs::File::open(dir.join("f")).expect("open");
        assert!(entry_names(file.as_raw_fd()).is_err());
        let d = std::fs::File::open(&*dir).expect("dir");
        let mut names: Vec<_> = entry_names(d.as_raw_fd())
            .expect("names")
            .iter()
            .map(|n| n.to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["d", "f"]);
    }

    /// Catches a walk that goes on after an entry it cannot open: a directory with no
    /// read permission is an error naming the tree, not a silent skip. (As root the
    /// directory opens, so the walk succeeds.)
    #[test]
    fn a_directory_that_cannot_be_opened_is_an_error() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("hostfs-sealed");
        let root = dir.join("t");
        std::fs::create_dir_all(root.join("sealed")).expect("dirs");
        std::fs::set_permissions(root.join("sealed"), std::fs::Permissions::from_mode(0o000))
            .expect("seal");
        let failed = chown_tree(&root, me(), my_group()).is_err();
        std::fs::set_permissions(root.join("sealed"), std::fs::Permissions::from_mode(0o700))
            .expect("unseal");
        assert_eq!(failed, me() != 0);
    }
}
