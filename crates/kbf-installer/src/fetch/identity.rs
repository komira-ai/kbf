//! The node's identity, lent to the fetch child (contract section 5, "Identity").
//!
//! For the `frontend` release source the child needs the node's client certificate and key
//! for the mutual TLS of `ReleaseService.Fetch`, and the pinned CA that authenticates the
//! frontend. The root parent reads the three PEM files itself (each opened without following
//! a link in any component of its path, a regular file with one hard link, at most
//! [`MAX_PEM_BYTES`]) and lends them as one small blob over a
//! descriptor, never on disk: on Linux a sealed anonymous memory file, elsewhere a socket pair
//! fed by a thread. The child reads the blob to its end, once.
//!
//! **A compromised child can steal the node key.** That is the accepted v1 residual of the
//! contract (a seven-day certificate); the follow-up is a fetch-scoped certificate.

use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// The most one PEM file may be.
pub const MAX_PEM_BYTES: u64 = 32 * 1024;
/// The most the lent blob may be: three files and their length prefixes.
pub const MAX_IDENTITY_BYTES: usize = 3 * (MAX_PEM_BYTES as usize + 4);

/// The certificate, key and CA, as PEM text.
#[derive(Clone, PartialEq, Eq)]
pub struct Identity {
    pub cert: Vec<u8>,
    pub key: Vec<u8>,
    pub ca: Vec<u8>,
}

// The key is never printed, not even in a failed assertion.
impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Identity { .. }")
    }
}

/// Why an identity could not be read or lent.
#[derive(Debug, thiserror::Error)]
pub enum IdentityFault {
    #[error("cannot read {}: {why}", path.display())]
    File { path: PathBuf, why: String },
    #[error("cannot lend the identity: {0}")]
    Lend(#[from] io::Error),
    #[error("the lent identity is malformed: {0}")]
    Malformed(&'static str),
    #[error("the child holds descriptor {0}, which nobody lent it")]
    Stray(i32),
}

/// Read one PEM file for lending: an absolute path, opened without following a link in any
/// component (see [`open_plain`]), a regular file with one hard link, not empty, at most
/// [`MAX_PEM_BYTES`]. The files are written by enrollment, as `kbf`, who is the party these
/// checks are for: it owns the directories, so it could swap one for a link, or hard-link a
/// file it cannot read, to have root read a file of someone else's and lend it. Who owns the
/// file is not asked (the owner is not fixed yet).
pub fn read_pem(path: &Path) -> Result<Vec<u8>, IdentityFault> {
    use std::os::unix::fs::MetadataExt as _;
    let fault = |why: String| IdentityFault::File {
        path: path.to_path_buf(),
        why,
    };
    if !path.is_absolute() {
        return Err(fault("not an absolute path".to_owned()));
    }
    let file = open_plain(path).map_err(|e| fault(e.to_string()))?;
    let meta = file.metadata().map_err(|e| fault(e.to_string()))?;
    if !meta.is_file() {
        return Err(fault("not a regular file".to_owned()));
    }
    if meta.nlink() > 1 {
        return Err(fault("has more than one hard link".to_owned()));
    }
    // One more than the most, so a file that is too large is seen and not silently cut.
    let mut bytes = Vec::new();
    let mut reader = file.take(MAX_PEM_BYTES + 1);
    reader
        .read_to_end(&mut bytes)
        .map_err(|e| fault(e.to_string()))?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_PEM_BYTES {
        return Err(fault(format!(
            "empty or over {MAX_PEM_BYTES} bytes; at least 1 and at most {MAX_PEM_BYTES}"
        )));
    }
    Ok(bytes)
}

/// Open the absolute `path` for reading, refusing a link in any component of it. On Linux that
/// is `openat2` with `NO_SYMLINKS` and `NO_MAGICLINKS`, which resolves the whole path itself
/// (no race with a directory swapped between two steps). Where the call is not there or not
/// allowed (before Linux 5.6, a sandbox that filters it) or off Linux, only the last component
/// is checked, as the order spool does ([`crate::spool::OpenMode`]); a link in a directory above
/// it is then followed. Not blocking, so a FIFO is not waited for.
#[cfg(target_os = "linux")]
fn open_plain(path: &Path) -> io::Result<File> {
    open_plain_with(path, &|path| {
        use rustix::fs::{CWD, Mode, OFlags, ResolveFlags, openat2};
        let flags = OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let resolve = ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS;
        openat2(CWD, path, flags, Mode::empty(), resolve)
    })
}

/// [`open_plain`] given the resolving open, so a test can hand it one that is not there. A
/// `&dyn`, not a generic: there is one copy of this function, and the tests that hand it other
/// opens cover the arms the real one does not reach here.
#[cfg(target_os = "linux")]
fn open_plain_with(
    path: &Path,
    resolve: &dyn Fn(&Path) -> rustix::io::Result<OwnedFd>,
) -> io::Result<File> {
    use rustix::io::Errno;
    match resolve(path) {
        Ok(fd) => Ok(File::from(fd)),
        Err(Errno::NOSYS | Errno::PERM) => open_last_not_followed(path),
        Err(e) => Err(e.into()),
    }
}

#[cfg(not(target_os = "linux"))]
fn open_plain(path: &Path) -> io::Result<File> {
    open_last_not_followed(path)
}

/// Open `path` for reading without following a link as its last component.
fn open_last_not_followed(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
}

impl Identity {
    /// Read the three files (each at fetch time, so a renewed certificate is used).
    pub fn read(cert: &Path, key: &Path, ca: &Path) -> Result<Identity, IdentityFault> {
        Ok(Identity {
            cert: read_pem(cert)?,
            key: read_pem(key)?,
            ca: read_pem(ca)?,
        })
    }

    /// Three sections, each a big-endian `u32` length and the bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for part in [&self.cert, &self.key, &self.ca] {
            out.extend_from_slice(&(part.len() as u32).to_be_bytes());
            out.extend_from_slice(part);
        }
        out
    }

    /// Read what [`Identity::encode`] wrote, from the descriptor the child was lent. Reads to
    /// the end and refuses anything over [`MAX_IDENTITY_BYTES`], a section over
    /// [`MAX_PEM_BYTES`], a short section and trailing bytes.
    pub fn decode(from: impl Read) -> Result<Identity, IdentityFault> {
        let mut blob = Vec::new();
        from.take(MAX_IDENTITY_BYTES as u64 + 1)
            .read_to_end(&mut blob)
            .map_err(IdentityFault::Lend)?;
        if blob.len() > MAX_IDENTITY_BYTES {
            return Err(IdentityFault::Malformed("too large"));
        }
        let mut rest = blob.as_slice();
        let mut parts = Vec::with_capacity(3);
        for _ in 0..3 {
            let (len, tail) = rest
                .split_first_chunk::<4>()
                .ok_or(IdentityFault::Malformed("a length is cut short"))?;
            let len = u32::from_be_bytes(*len) as usize;
            if len == 0 || len as u64 > MAX_PEM_BYTES || len > tail.len() {
                return Err(IdentityFault::Malformed(
                    "a section is empty, too large or cut short",
                ));
            }
            let (part, tail) = tail.split_at(len);
            parts.push(part.to_vec());
            rest = tail;
        }
        if !rest.is_empty() {
            return Err(IdentityFault::Malformed("bytes after the third section"));
        }
        let ca = parts.pop().expect("three parts");
        let key = parts.pop().expect("three parts");
        let cert = parts.pop().expect("three parts");
        Ok(Identity { cert, key, ca })
    }

    /// A descriptor the child can read the identity from. Close-on-exec in this process (the
    /// start of the child clears that on the one descriptor it keeps); never a file on disk.
    #[cfg(target_os = "linux")]
    pub fn lend(&self) -> Result<OwnedFd, IdentityFault> {
        use rustix::fs::{MemfdFlags, SealFlags, SeekFrom, fcntl_add_seals, memfd_create, seek};
        let fd = memfd_create(
            "kbf-fetch-identity",
            MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
        )
        .map_err(io::Error::from)?;
        let blob = self.encode();
        let mut written = 0;
        while written < blob.len() {
            written += rustix::io::write(&fd, &blob[written..]).map_err(io::Error::from)?;
        }
        // The child shares this open file description: it starts reading at the front, and
        // cannot change what it reads.
        seek(&fd, SeekFrom::Start(0)).map_err(io::Error::from)?;
        fcntl_add_seals(
            &fd,
            SealFlags::WRITE | SealFlags::GROW | SealFlags::SHRINK | SealFlags::SEAL,
        )
        .map_err(io::Error::from)?;
        Ok(fd)
    }

    /// A descriptor the child can read the identity from: one end of a socket pair that a
    /// thread fills and closes (a socket buffer may be smaller than the blob). Never a file.
    #[cfg(not(target_os = "linux"))]
    pub fn lend(&self) -> Result<OwnedFd, IdentityFault> {
        use std::io::Write;
        use std::os::unix::net::UnixStream;
        let (mut near, far) = UnixStream::pair().map_err(IdentityFault::Lend)?;
        let blob = self.encode();
        std::thread::spawn(move || {
            let _ = near.write_all(&blob);
        });
        Ok(OwnedFd::from(far))
    }
}

/// In the child: take the lent descriptor `fd` as an owned file, and make it descriptor 4,
/// the number the contract names (the parent lends it at any number, because a number chosen
/// beforehand could be the one the standard library's own start-up pipe already holds).
/// Afterwards the child must hold no descriptor but 0, 1, 2 and 4, and fails if it does.
pub fn adopt(fd: i32) -> Result<File, IdentityFault> {
    const FOUR: i32 = 4;
    if fd < 3 {
        return Err(IdentityFault::Malformed(
            "the identity descriptor is a standard stream",
        ));
    }
    if fd != FOUR {
        // SAFETY: dup2 and close on descriptors of this process; at this point only 0, 1, 2
        // and `fd` are open (the parent marked the rest close-on-exec), which is checked below.
        unsafe {
            if libc::dup2(fd, FOUR) < 0 {
                return Err(IdentityFault::Lend(io::Error::last_os_error()));
            }
            libc::close(fd);
        }
    }
    ensure_descriptors(&[0, 1, 2, FOUR])?;
    // SAFETY: descriptor 4 is open and not owned by anything else in this process.
    Ok(unsafe { File::from_raw_fd(FOUR) })
}

/// In the child: fail if it holds a descriptor outside `allowed`. The parent marks everything
/// but the standard streams and the lent identity close-on-exec; this is the child checking
/// that the parent did.
pub fn ensure_descriptors(allowed: &[i32]) -> Result<(), IdentityFault> {
    let open = open_descriptors().map_err(IdentityFault::Lend)?;
    match open.iter().find(|n| !allowed.contains(n)) {
        Some(extra) => Err(IdentityFault::Stray(*extra)),
        None => Ok(()),
    }
}

#[cfg(target_os = "linux")]
const FD_DIR: &str = "/proc/self/fd";
#[cfg(not(target_os = "linux"))]
const FD_DIR: &str = "/dev/fd";

/// The descriptors this process has open. They are listed, the listing closed, and those still
/// open kept, so the descriptor the listing itself used is not among them.
fn open_descriptors() -> io::Result<Vec<i32>> {
    let mut listed = Vec::new();
    for entry in std::fs::read_dir(FD_DIR)? {
        let name = entry?.file_name();
        listed.extend(name.to_str().and_then(|n| n.parse::<i32>().ok()));
    }
    // SAFETY: F_GETFD on a number only reports whether it is open.
    listed.retain(|n| unsafe { libc::fcntl(*n, libc::F_GETFD) } >= 0);
    Ok(listed)
}

#[cfg(test)]
#[path = "identity_tests.rs"]
mod tests;
