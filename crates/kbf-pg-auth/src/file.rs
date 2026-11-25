//! The password file a program is given: checked as kbf's other secret files are, on the open
//! file, so what is checked is what is read.

use std::io::Read as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::Path;

use rustix::fs::OFlags;
use zeroize::Zeroizing;

/// The most bytes a password file may hold, its line ending included.
pub const MAX_PASSWORD_FILE_BYTES: usize = 4096;

/// What is wrong with a password file. No text holds a byte of its content.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FileFlaw {
    /// It cannot be opened or read; the text is the system's.
    #[error("{0}")]
    Read(String),
    /// It is a symbolic link, which is not followed.
    #[error("it is a symbolic link")]
    Symlink,
    /// It is not a regular file.
    #[error("it is not a regular file")]
    NotAFile,
    /// Another user owns it.
    #[error("it is owned by uid {owner}, not by this process's uid {euid}")]
    Owner {
        /// Its owner.
        owner: u32,
        /// This process's effective uid.
        euid: u32,
    },
    /// Others than its owner may read or write it, or it is executable or more.
    #[error("it has mode {0:04o}; it must be 0600 or 0400")]
    Mode(u32),
    /// It holds more than [`MAX_PASSWORD_FILE_BYTES`].
    #[error("it holds more than {MAX_PASSWORD_FILE_BYTES} bytes")]
    TooLarge,
    /// It holds no password, or more than one line, or a NUL byte.
    #[error("it must hold the password alone, on one line")]
    Content,
}

/// The password in the file at `path`, which `euid` must own (any owner when `euid` is root's):
/// its content less one line ending (`\n` or `\r\n`).
///
/// # Errors
/// [`FileFlaw`].
pub(crate) fn read(path: &Path, euid: u32) -> Result<Zeroizing<Vec<u8>>, FileFlaw> {
    // Non-blocking, so that a FIFO is refused at once rather than waited on; a symbolic link
    // is not followed.
    let flags = (OFlags::NONBLOCK | OFlags::NOFOLLOW).bits().cast_signed();
    // One mapping for every step: only the open fails in practice (a link is ELOOP there).
    let unreadable = |e: std::io::Error| {
        if e.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) {
            FileFlaw::Symlink
        } else {
            FileFlaw::Read(e.to_string())
        }
    };
    let mut open = std::fs::OpenOptions::new();
    let file = open
        .read(true)
        .custom_flags(flags)
        .open(path)
        .map_err(unreadable)?;
    let meta = file.metadata().map_err(unreadable)?;
    if !meta.is_file() {
        return Err(FileFlaw::NotAFile);
    }
    if meta.uid() != euid && euid != 0 {
        return Err(FileFlaw::Owner {
            owner: meta.uid(),
            euid,
        });
    }
    let mode = meta.mode() & 0o7777;
    if mode != 0o600 && mode != 0o400 {
        return Err(FileFlaw::Mode(mode));
    }
    let mut content = Zeroizing::new(Vec::new());
    // One byte past the limit is read, so that a file over it is refused whatever its length
    // said when it was opened.
    let mut limited = file.take(MAX_PASSWORD_FILE_BYTES as u64 + 1);
    limited.read_to_end(&mut content).map_err(unreadable)?;
    if content.len() > MAX_PASSWORD_FILE_BYTES {
        return Err(FileFlaw::TooLarge);
    }
    let line = content
        .strip_suffix(b"\r\n")
        .or_else(|| content.strip_suffix(b"\n"))
        .unwrap_or(&content);
    if line.is_empty() || line.iter().any(|b| matches!(b, b'\n' | b'\r' | 0)) {
        return Err(FileFlaw::Content);
    }
    Ok(Zeroizing::new(line.to_vec()))
}

#[cfg(test)]
#[path = "file_tests.rs"]
pub(crate) mod tests;
