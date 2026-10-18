//! Files and directories moved by descriptor: the copy of an old release directory into
//! the new tree, the digest that tells two copies are the same, and the small reads and
//! atomic writes the migration does in directories it holds open.
//!
//! Nothing here resolves a path twice. A directory is opened once, each name below it is
//! opened `openat` relative to the directory above, with `O_NOFOLLOW`, and what was opened
//! is judged by `fstat` of the descriptor: a link (ELOOP), a FIFO, socket or device, a
//! file with a second hard link, a file or directory not owned by the expected user, a
//! setuid or setgid file, one writable by group or other, and (Linux) one with file
//! capabilities are all refused, so a release is never copied out of something an
//! unprivileged user could have swapped. A copy is made with fresh modes (files 0755 when
//! the source was executable by its owner, else 0644; directories 0755), never the
//! source's, and never any extended attribute.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::path::Path;

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, RawMode, Stat};
use rustix::io::Errno;
use sha2::{Digest, Sha256};

/// The extended attribute that gives a file capabilities.
pub const CAPABILITY_XATTR: &str = "security.capability";

/// What a tree may hold, and who must own it.
#[derive(Clone, Copy, Debug)]
pub struct Rules {
    /// Every file and directory is owned by this user (root on a host).
    pub owner_uid: u32,
    /// No file bigger than this.
    pub max_file_bytes: u64,
    /// No more entries (files and directories) than this in all.
    pub max_entries: usize,
    /// No directory nested deeper than this below the top.
    pub max_depth: usize,
    /// No file with this extended attribute.
    pub denied_xattr: &'static str,
    /// Names the top of the tree may not hold: a copy must not mint what only a check
    /// writes (the `VERIFIED` marker).
    pub reserved: &'static [&'static str],
}

impl Rules {
    /// The rules for a release: a binary is at most 1 GiB (as an archive member is), and a
    /// release is a few files and a `units` directory.
    pub fn release(owner_uid: u32) -> Rules {
        Rules {
            owner_uid,
            max_file_bytes: 1 << 30,
            max_entries: 4096,
            max_depth: 8,
            denied_xattr: CAPABILITY_XATTR,
            reserved: &["VERIFIED"],
        }
    }
}

/// Why a tree was not copied, read or written.
#[derive(Debug, thiserror::Error)]
pub enum TreeError {
    /// A rule of [`Rules`] (or the kind of thing a name is) refuses it.
    #[error("{path}: {why}")]
    Refused { path: String, why: &'static str },
    #[error("{path}: {source}")]
    Io { path: String, source: io::Error },
}

fn refuse(path: &str, why: &'static str) -> TreeError {
    TreeError::Refused {
        path: path.to_owned(),
        why,
    }
}

/// An error of an operation on `path`.
trait At<T> {
    fn at(self, path: &str) -> Result<T, TreeError>;
}

impl<T, E: Into<io::Error>> At<T> for Result<T, E> {
    fn at(self, path: &str) -> Result<T, TreeError> {
        self.map_err(|e| TreeError::Io {
            path: path.to_owned(),
            source: e.into(),
        })
    }
}

/// Open `dir` itself (a path chosen by root, not by an untrusted writer).
pub fn open_root(dir: &Path) -> io::Result<OwnedFd> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    Ok(rustix::fs::open(dir, flags, Mode::empty())?)
}

/// Open the directory `name` of `parent`, never through a link.
pub fn open_dir(parent: &OwnedFd, name: &str) -> io::Result<OwnedFd> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    Ok(rustix::fs::openat(parent, name, flags, Mode::empty())?)
}

/// Refuse a directory `fd` the rules do not accept (not a directory, not owned by the
/// expected user, writable by group or other).
pub fn expect_dir(rules: &Rules, fd: &OwnedFd, path: &str) -> Result<(), TreeError> {
    let st = rustix::fs::fstat(fd).at(path)?;
    match judge(rules, fd, &st, path)? {
        FileType::Directory => Ok(()),
        _ => Err(refuse(path, "not a directory")),
    }
}

/// Whether the file has the extended attribute `name`. A file system without extended
/// attributes has no capabilities either.
#[cfg(target_os = "linux")]
fn has_xattr(fd: &OwnedFd, name: &str) -> io::Result<bool> {
    match rustix::fs::fgetxattr(fd, name, &mut [0u8; 0]) {
        Ok(_) => Ok(true),
        Err(Errno::NODATA | Errno::OPNOTSUPP) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// File capabilities are a Linux feature.
#[cfg(not(target_os = "linux"))]
fn has_xattr(_fd: &OwnedFd, _name: &str) -> io::Result<bool> {
    Ok(false)
}

/// What an open entry is, and whether the rules accept it.
fn judge(rules: &Rules, fd: &OwnedFd, st: &Stat, path: &str) -> Result<FileType, TreeError> {
    let kind = FileType::from_raw_mode(st.st_mode);
    let mode = Mode::from_raw_mode(st.st_mode);
    if kind != FileType::RegularFile && kind != FileType::Directory {
        return Err(refuse(path, "not a regular file or a directory"));
    }
    if st.st_uid != rules.owner_uid {
        return Err(refuse(path, "not owned by the expected user"));
    }
    if mode.intersects(Mode::WGRP | Mode::WOTH) {
        return Err(refuse(path, "writable by group or other"));
    }
    if kind == FileType::Directory {
        return Ok(kind);
    }
    if mode.intersects(Mode::SUID | Mode::SGID) {
        return Err(refuse(path, "setuid or setgid"));
    }
    // `st_nlink` is a u64 on x86-64 and a u16 on macOS: compare with a literal.
    if st.st_nlink != 1 {
        return Err(refuse(path, "has another hard link"));
    }
    if has_xattr(fd, rules.denied_xattr).at(path)? {
        return Err(refuse(path, "has file capabilities"));
    }
    Ok(kind)
}

/// The names in the directory `dir`, without `.` and `..`, in byte order.
fn names(dir: &OwnedFd, path: &str) -> Result<Vec<CString>, TreeError> {
    let mut all = Vec::new();
    for item in Dir::read_from(dir).at(path)? {
        let name = item.at(path)?.file_name().to_owned();
        if name.as_bytes() != b"." && name.as_bytes() != b".." {
            all.push(name);
        }
    }
    all.sort();
    Ok(all)
}

/// Writes what is read to the digest and, when copying, to the new file.
struct Tee<'a> {
    inner: &'a mut Sha256,
    out: Option<File>,
    total: u64,
}

impl Write for Tee<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.update(buf);
        self.total += buf.len() as u64;
        if let Some(out) = &mut self.out {
            out.write_all(buf)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Walk<'a> {
    rules: &'a Rules,
    entries: usize,
    digest: Sha256,
}

impl Walk<'_> {
    /// Visit the entries of `src`, copying each into `dst` when there is one.
    fn dir(
        &mut self,
        src: &OwnedFd,
        dst: Option<&OwnedFd>,
        path: &str,
        depth: usize,
    ) -> Result<(), TreeError> {
        if depth > self.rules.max_depth {
            return Err(refuse(path, "nested too deep"));
        }
        for name in names(src, path)? {
            if depth == 0
                && self
                    .rules
                    .reserved
                    .iter()
                    .any(|r| r.as_bytes() == name.to_bytes())
            {
                return Err(refuse(
                    &name.to_string_lossy(),
                    "a name only a check may write",
                ));
            }
            self.entries += 1;
            if self.entries > self.rules.max_entries {
                return Err(refuse(path, "too many entries"));
            }
            let shown = format!("{path}/{}", name.to_string_lossy());
            self.entry(src, dst, &name, &shown, depth)?;
        }
        Ok(())
    }

    fn entry(
        &mut self,
        src: &OwnedFd,
        dst: Option<&OwnedFd>,
        name: &CStr,
        path: &str,
        depth: usize,
    ) -> Result<(), TreeError> {
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let fd = rustix::fs::openat(src, name, flags, Mode::empty()).at(path)?;
        let st = rustix::fs::fstat(&fd).at(path)?;
        let kind = judge(self.rules, &fd, &st, path)?;
        self.digest.update([u8::from(kind == FileType::Directory)]);
        self.digest
            .update((name.to_bytes().len() as u64).to_le_bytes());
        self.digest.update(name.to_bytes());
        if kind == FileType::Directory {
            let made = dst.map(|d| make_dir(d, name, path)).transpose()?;
            self.dir(&fd, made.as_ref(), path, depth + 1)?;
            self.digest.update([0xff]);
            return finish(made, 0o755, path);
        }
        let executable = Mode::from_raw_mode(st.st_mode).contains(Mode::XUSR);
        let out = dst.map(|d| create(d, name, path)).transpose()?;
        let mut inner = Sha256::new();
        let mut tee = Tee {
            inner: &mut inner,
            out,
            total: 0,
        };
        let mut input = File::from(fd).take(self.rules.max_file_bytes + 1);
        io::copy(&mut input, &mut tee).at(path)?;
        let (total, out) = (tee.total, tee.out);
        if total > self.rules.max_file_bytes {
            return Err(refuse(path, "bigger than the file size cap"));
        }
        self.digest.update([u8::from(executable)]);
        self.digest.update(total.to_le_bytes());
        self.digest.update(inner.finalize());
        finish(
            out.map(OwnedFd::from),
            if executable { 0o755 } else { 0o644 },
            path,
        )
    }
}

/// Make the directory `name` in `parent` (0700 until it is filled) and open it.
fn make_dir(parent: &OwnedFd, name: &CStr, path: &str) -> Result<OwnedFd, TreeError> {
    rustix::fs::mkdirat(parent, name, Mode::from_raw_mode(0o700)).at(path)?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    rustix::fs::openat(parent, name, flags, Mode::empty()).at(path)
}

/// Make the file `name` in `parent`, which must not be there, and open it for writing.
fn create(parent: &OwnedFd, name: &CStr, path: &str) -> Result<File, TreeError> {
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let fd = rustix::fs::openat(parent, name, flags, Mode::from_raw_mode(0o600)).at(path)?;
    Ok(File::from(fd))
}

/// Give a copy its final mode and put it on disk.
fn finish(fd: Option<OwnedFd>, mode: RawMode, path: &str) -> Result<(), TreeError> {
    if let Some(fd) = fd {
        rustix::fs::fchmod(&fd, Mode::from_raw_mode(mode)).at(path)?;
        rustix::fs::fsync(&fd).at(path)?;
    }
    Ok(())
}

fn run(rules: &Rules, src: &OwnedFd, dst: Option<&OwnedFd>) -> Result<[u8; 32], TreeError> {
    let st = rustix::fs::fstat(src).at(".")?;
    if judge(rules, src, &st, ".")? != FileType::Directory {
        return Err(refuse(".", "not a directory"));
    }
    let mut walk = Walk {
        rules,
        entries: 0,
        digest: Sha256::new(),
    };
    walk.dir(src, dst, "", 0)?;
    Ok(walk.digest.finalize().into())
}

/// A digest of the tree below `dir`: names, kinds, whether a file is executable, and
/// contents. Two trees with the same digest are the same release, whatever their modes
/// (other than the executable bit), owners and times.
pub fn digest(rules: &Rules, dir: &OwnedFd) -> Result<[u8; 32], TreeError> {
    run(rules, dir, None)
}

/// Copy the tree below `src` into the empty directory `dst`, and return the digest of what
/// was read (so the caller can compare it with a digest of the copy).
pub fn copy(rules: &Rules, src: &OwnedFd, dst: &OwnedFd) -> Result<[u8; 32], TreeError> {
    run(rules, src, Some(dst))
}

/// The bytes of the regular file `name` of `dir`, judged by the rules; `None` when there
/// is no such name. At most `cap` bytes.
pub fn read_file(
    rules: &Rules,
    dir: &OwnedFd,
    name: &str,
    cap: u64,
) -> Result<Option<Vec<u8>>, TreeError> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd = match rustix::fs::openat(dir, name, flags, Mode::empty()) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(None),
        Err(e) => return Err(e).at(name),
    };
    let st = rustix::fs::fstat(&fd).at(name)?;
    if judge(rules, &fd, &st, name)? != FileType::RegularFile {
        return Err(refuse(name, "not a regular file"));
    }
    let mut bytes = Vec::new();
    File::from(fd)
        .take(cap + 1)
        .read_to_end(&mut bytes)
        .at(name)?;
    if bytes.len() as u64 > cap {
        return Err(refuse(name, "bigger than the size cap"));
    }
    Ok(Some(bytes))
}

/// Replace the file `name` of `dir` with `bytes` (mode `mode`): written whole to a new
/// name beside it, synced, renamed over it, and the directory synced. A leftover of an
/// interrupted run under the new name is replaced.
pub fn replace_file(
    dir: &OwnedFd,
    name: &str,
    mode: RawMode,
    bytes: &[u8],
) -> Result<(), TreeError> {
    let temp = format!(".{name}.new");
    match rustix::fs::unlinkat(dir, &temp, AtFlags::empty()) {
        Ok(()) | Err(Errno::NOENT) => {}
        Err(e) => return Err(e).at(&temp),
    }
    let mut file = create(dir, &CString::new(temp.clone()).expect("no NUL"), &temp)?;
    file.write_all(bytes).at(&temp)?;
    rustix::fs::fchmod(&file, Mode::from_raw_mode(mode)).at(&temp)?;
    file.sync_all().at(&temp)?;
    rustix::fs::renameat(dir, &temp, dir, name).at(name)?;
    rustix::fs::fsync(dir).at(name)
}

/// Remove the entry `name` of `parent` and everything below it, never through a link (a
/// link is removed, not followed). A tree nested deeper than `MAX_DEPTH` is an error.
pub fn remove_tree(parent: &OwnedFd, name: &str) -> io::Result<()> {
    let name = CString::new(name).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    remove_at(parent, &name, 0)
}

/// The deepest tree [`remove_tree`] removes.
const MAX_DEPTH: usize = 16;

fn remove_at(parent: &OwnedFd, name: &CStr, depth: usize) -> io::Result<()> {
    let st = rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
    if FileType::from_raw_mode(st.st_mode) != FileType::Directory {
        return Ok(rustix::fs::unlinkat(parent, name, AtFlags::empty())?);
    }
    if depth >= MAX_DEPTH {
        return Err(io::Error::other("nested too deep to remove"));
    }
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let dir = rustix::fs::openat(parent, name, flags, Mode::empty())?;
    for item in Dir::read_from(&dir)? {
        let item = item?.file_name().to_owned();
        if item.as_bytes() != b"." && item.as_bytes() != b".." {
            remove_at(&dir, &item, depth + 1)?;
        }
    }
    Ok(rustix::fs::unlinkat(parent, name, AtFlags::REMOVEDIR)?)
}

#[cfg(test)]
#[path = "tree_tests.rs"]
mod tests;
