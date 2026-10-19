//! The proxy's secret files: reading one the way the child does (`kbf-server`'s `token.rs`),
//! and writing one so a crash never leaves a half file the child would refuse for ever.
//!
//! A file is trusted only if it is a regular file, owned by this process's effective user,
//! with mode 0600 or 0400, and is checked on the open handle, so what is checked is what is
//! read. A file that anyone else could have written, or read, is refused, not repaired.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// Why a secret file is refused.
#[derive(Debug, thiserror::Error)]
pub enum FileError {
    /// It cannot be opened or read.
    #[error("read {path}: {source}")]
    Read {
        /// The file.
        path: PathBuf,
        /// The reason.
        #[source]
        source: io::Error,
    },
    /// It is not a regular file.
    #[error("{path} is not a regular file")]
    NotAFile {
        /// The file.
        path: PathBuf,
    },
    /// Another user owns it.
    #[error("{path} is owned by uid {owner}, not by this process's uid {server}")]
    Owner {
        /// The file.
        path: PathBuf,
        /// Its owner.
        owner: u32,
        /// The effective user of this process.
        server: u32,
    },
    /// Its mode lets someone other than its owner read or write it, or is more than read and
    /// write.
    #[error("{path} has mode {mode:04o}; it must be 0600 or 0400")]
    Mode {
        /// The file.
        path: PathBuf,
        /// Its mode.
        mode: u32,
    },
}

impl FileError {
    /// Whether the file is simply not there.
    pub(crate) fn is_missing(&self) -> bool {
        matches!(self, Self::Read { source, .. } if source.kind() == io::ErrorKind::NotFound)
    }
}

impl From<FileError> for io::Error {
    fn from(error: FileError) -> Self {
        let kind = match &error {
            FileError::Read { source, .. } => source.kind(),
            FileError::NotAFile { .. } => io::ErrorKind::InvalidInput,
            FileError::Owner { .. } | FileError::Mode { .. } => io::ErrorKind::PermissionDenied,
        };
        Self::new(kind, error)
    }
}

/// The user this process runs as.
pub(crate) fn effective_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// Whether a file with this type, owner and mode may be read as a secret by `server`.
pub(crate) fn check(
    path: &Path,
    is_file: bool,
    owner: u32,
    mode: u32,
    server: u32,
) -> Result<(), FileError> {
    if !is_file {
        return Err(FileError::NotAFile {
            path: path.to_owned(),
        });
    }
    if owner != server {
        return Err(FileError::Owner {
            path: path.to_owned(),
            owner,
            server,
        });
    }
    let mode = mode & 0o7777;
    if mode != 0o600 && mode != 0o400 {
        return Err(FileError::Mode {
            path: path.to_owned(),
            mode,
        });
    }
    Ok(())
}

/// The content of the file at `path` under the rules above: at most `limit + 1` bytes, so a
/// caller can tell a file over `limit` by its length. Opened without blocking, so a FIFO
/// does not wait for a writer.
pub(crate) fn read_owner_only(path: &Path, limit: usize) -> Result<Vec<u8>, FileError> {
    let read = |source| FileError::Read {
        path: path.to_owned(),
        source,
    };
    let nonblocking = rustix::fs::OFlags::NONBLOCK.bits().cast_signed();
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nonblocking)
        .open(path)
        .map_err(read)?;
    let meta = file.metadata().map_err(read)?;
    check(
        path,
        meta.is_file(),
        meta.uid(),
        meta.mode(),
        effective_uid(),
    )?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(read)?;
    Ok(bytes)
}

/// A temporary file that removes itself, so a failed write leaves nothing behind.
struct Aside(PathBuf);

impl Drop for Aside {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// How [`write_new`] puts the file at its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Place {
    /// `link(2)`: fails if the name is taken, so two proxies never both create.
    New,
    /// `rename(2)`: replaces what is there, atomically.
    Replace,
}

/// Writes `bytes` to a temporary file beside `path` (mode 0600, no trailing newline or
/// anything else), fsyncs it, and puts it at `path` with [`Place`]: a crash leaves the old
/// file or the whole new one, never an empty one. Then fsyncs the directory.
pub(crate) fn write_new(path: &Path, bytes: &[u8], place: Place) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let aside = Aside(dir.join(format!(".{name}.{}.tmp", std::process::id())));
    // A temporary file of a process that crashed under the same pid.
    let _ = std::fs::remove_file(&aside.0);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&aside.0)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    match place {
        Place::New => std::fs::hard_link(&aside.0, path)?,
        Place::Replace => std::fs::rename(&aside.0, path)?,
    }
    File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::scratch::{scratch, write_file};

    #[test]
    fn a_file_is_trusted_only_when_it_is_this_users_and_closed() {
        let path = Path::new("/secret");
        assert!(check(path, true, 7, 0o600, 7).is_ok());
        assert!(
            check(path, true, 7, 0o100_400, 7).is_ok(),
            "the file type bits are not the mode"
        );
        assert!(matches!(
            check(path, false, 7, 0o600, 7),
            Err(FileError::NotAFile { .. })
        ));
        assert!(matches!(
            check(path, true, 8, 0o600, 7),
            Err(FileError::Owner {
                owner: 8,
                server: 7,
                ..
            })
        ));
        for mode in [0o644, 0o640, 0o660, 0o700, 0o000, 0o4600, 0o200] {
            assert!(
                matches!(check(path, true, 7, mode, 7), Err(FileError::Mode { .. })),
                "{mode:o}"
            );
        }
    }

    #[test]
    fn a_refusal_names_the_file_and_the_rule() {
        let path = PathBuf::from("/run/kbf-proxy/proxy-auth");
        let text = |e: FileError| e.to_string();
        assert!(
            text(FileError::Mode {
                path: path.clone(),
                mode: 0o644
            })
            .contains("0644")
        );
        assert!(
            text(FileError::Owner {
                path: path.clone(),
                owner: 1,
                server: 2
            })
            .contains("uid 1")
        );
        assert!(text(FileError::NotAFile { path: path.clone() }).contains("regular"));
        let read = FileError::Read {
            path,
            source: io::ErrorKind::NotFound.into(),
        };
        assert!(read.is_missing());
        let denied = FileError::Read {
            path: PathBuf::new(),
            source: io::ErrorKind::PermissionDenied.into(),
        };
        assert!(!denied.is_missing());
        assert!(
            !FileError::NotAFile {
                path: PathBuf::new()
            }
            .is_missing()
        );
        let kinds = [
            (read, io::ErrorKind::NotFound),
            (
                FileError::NotAFile {
                    path: PathBuf::new(),
                },
                io::ErrorKind::InvalidInput,
            ),
            (
                FileError::Mode {
                    path: PathBuf::new(),
                    mode: 0,
                },
                io::ErrorKind::PermissionDenied,
            ),
            (
                FileError::Owner {
                    path: PathBuf::new(),
                    owner: 0,
                    server: 1,
                },
                io::ErrorKind::PermissionDenied,
            ),
        ];
        for (error, kind) in kinds {
            assert_eq!(io::Error::from(error).kind(), kind);
        }
    }

    #[test]
    fn a_file_is_read_up_to_one_byte_over_its_limit() {
        let dir = scratch("files-read");
        let path = dir.join("f");
        write_file(&path, b"0123456789", 0o600);
        assert_eq!(read_owner_only(&path, 32).expect("read"), b"0123456789");
        assert_eq!(read_owner_only(&path, 4).expect("read").len(), 5);
        write_file(&path, b"x", 0o400);
        assert!(read_owner_only(&path, 4).is_ok());
        write_file(&path, b"x", 0o644);
        assert!(matches!(
            read_owner_only(&path, 4),
            Err(FileError::Mode { .. })
        ));
        assert!(
            read_owner_only(&dir.join("absent"), 4)
                .expect_err("absent")
                .is_missing()
        );
        assert!(matches!(
            read_owner_only(&dir, 4),
            Err(FileError::NotAFile { .. })
        ));
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("list")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Both ways of placing the file leave exactly the bytes given, mode 0600, and no
    /// temporary file, whether the write works or not.
    #[test]
    fn a_secret_is_written_whole_and_closed_and_leaves_nothing_behind() {
        let dir = scratch("files-write");
        let path = dir.join("proxy-auth");
        write_new(&path, &[7; 32], Place::New).expect("create");
        assert_eq!(std::fs::read(&path).expect("read"), [7; 32]);
        assert_eq!(
            std::fs::metadata(&path).expect("meta").permissions().mode() & 0o7777,
            0o600
        );
        assert_eq!(entries(&dir), ["proxy-auth"]);

        // A name that is taken is not clobbered by a creation; the temporary file goes.
        let taken = write_new(&path, &[9; 32], Place::New).expect_err("the name is taken");
        assert_eq!(taken.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&path).expect("read"), [7; 32]);
        assert_eq!(entries(&dir), ["proxy-auth"]);

        // A temporary file left by a crashed process of the same pid does not stop a write.
        let stale = dir.join(format!(".proxy-auth.{}.tmp", std::process::id()));
        std::fs::write(&stale, b"half").expect("stale");
        write_new(&path, &[9; 32], Place::Replace).expect("replace");
        assert_eq!(std::fs::read(&path).expect("read"), [9; 32]);
        assert_eq!(entries(&dir), ["proxy-auth"]);

        // A directory that is not there fails and leaves nothing.
        assert!(write_new(&dir.join("absent").join("f"), &[1; 32], Place::New).is_err());
    }
}
