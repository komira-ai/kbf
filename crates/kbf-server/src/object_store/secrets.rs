//! The secret files: one per live connection, written atomically under a version in its
//! name, read only when mode 0600 or tighter.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use kbf_types::object_store_api::{ConnectionName, Secret};
use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::Step;

/// Any mode bit beyond the owner's read and write is too wide.
const WIDE_BITS: u32 = 0o177;

/// Temp files start with this, so a sweep can tell them from versioned files.
const TEMP_PREFIX: &str = ".tmp-";

/// Distinguishes the temp files of one process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Why a secret file could not be written or used. No variant carries the secret.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    /// The file the row names does not exist.
    #[error("the secret file is missing")]
    Missing,
    /// The file is readable or writable by someone besides its owner.
    #[error("the secret file has mode {0:04o}: it must be 0600")]
    WideMode(u32),
    /// The name is not a regular file (a link, a directory).
    #[error("the secret file is not a regular file")]
    NotRegular,
    /// The file is not the JSON this module writes.
    #[error("the secret file is not valid")]
    Invalid,
    /// A file system call failed.
    #[error("secret file {op}: {source}")]
    Io {
        /// What was being done.
        op: &'static str,
        /// The error of the call.
        source: io::Error,
    },
}

fn io_at(op: &'static str) -> impl FnOnce(io::Error) -> SecretError {
    move |source| SecretError::Io { op, source }
}

/// What a secret file holds. Written with a borrowed text so no second owner of the
/// secret exists; read into a [`Secret`].
#[derive(Serialize)]
struct StoredRef<'a> {
    secret_access_key: &'a str,
}

#[derive(Deserialize)]
struct Stored {
    secret_access_key: Secret,
}

/// The directory of secret files.
pub struct SecretFiles {
    dir: PathBuf,
    #[cfg(test)]
    failpoint: std::sync::Mutex<Option<Failpoint>>,
}

#[cfg(test)]
type Failpoint = std::sync::Arc<dyn Fn(Step) + Send + Sync>;

impl std::fmt::Debug for SecretFiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretFiles")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

impl SecretFiles {
    /// The files in `dir`, which is created (mode 0700, with its missing parents) if absent.
    ///
    /// # Errors
    /// The directory cannot be created.
    pub fn new(dir: &Path) -> Result<Self, SecretError> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(io_at("create the directory"))?;
        Ok(Self {
            dir: dir.to_owned(),
            #[cfg(test)]
            failpoint: std::sync::Mutex::new(None),
        })
    }

    /// Runs `hook` at each [`Step`] before it happens. A hook that panics is a crash at
    /// that step: nothing after it runs, not even cleanup.
    #[cfg(test)]
    pub(super) fn set_failpoint(&self, hook: impl Fn(Step) + Send + Sync + 'static) {
        *self
            .failpoint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(std::sync::Arc::new(hook));
    }

    /// The point where the write order passes `step`.
    pub(super) fn reach(&self, step: Step) {
        #[cfg(test)]
        {
            let hook = self
                .failpoint
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(hook) = hook {
                hook(step);
            }
        }
        #[cfg(not(test))]
        let _ = step;
    }

    /// The path of the file of `name` at `version`.
    pub(super) fn path(&self, name: &ConnectionName, version: u64) -> PathBuf {
        self.dir.join(format!("{name}.{version:016x}.json"))
    }

    /// Writes the file of `name` at `version`: temp file, fsync, rename, fsync of the
    /// directory. When it returns `Ok` the file is durable under its name; on `Err` no
    /// file of that name was left by this call.
    ///
    /// # Errors
    /// A file system call failed.
    pub(super) fn put(
        &self,
        name: &ConnectionName,
        version: u64,
        secret: &Secret,
    ) -> Result<(), SecretError> {
        let temp = self.dir.join(format!(
            "{TEMP_PREFIX}{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let result = self.put_through(&temp, &self.path(name, version), secret);
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    fn put_through(&self, temp: &Path, target: &Path, secret: &Secret) -> Result<(), SecretError> {
        // A struct of one string cannot fail to encode; were it to, the file would be empty
        // and every read of it `Invalid`, which fails closed.
        let bytes = Zeroizing::new(
            serde_json::to_vec(&StoredRef {
                secret_access_key: secret.expose(),
            })
            .unwrap_or_default(),
        );
        self.reach(Step::WriteTemp);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(temp)
            .map_err(io_at("create the temp file"))?;
        file.write_all(&bytes).map_err(io_at("write"))?;
        self.reach(Step::SyncTemp);
        file.sync_all().map_err(io_at("fsync the file"))?;
        drop(file);
        self.reach(Step::Rename);
        fs::rename(temp, target).map_err(io_at("rename"))?;
        self.reach(Step::SyncDir);
        File::open(&self.dir)
            .and_then(|dir| dir.sync_all())
            .map_err(io_at("fsync the directory"))
    }

    /// Reads the file of `name` at `version`.
    ///
    /// # Errors
    /// [`SecretError::Missing`] if there is none, [`SecretError::WideMode`] if its mode
    /// allows anyone but the owner (checked on the open file, so a swap cannot slip
    /// between the check and the read), [`SecretError::NotRegular`] for a link or a
    /// directory, [`SecretError::Invalid`] for anything but this module's JSON.
    pub(super) fn read(&self, name: &ConnectionName, version: u64) -> Result<Secret, SecretError> {
        self.reach(Step::ReadFile);
        let fd = rustix::fs::open(
            self.path(name, version),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|errno| match errno {
            Errno::NOENT => SecretError::Missing,
            // O_NOFOLLOW on a link.
            Errno::LOOP => SecretError::NotRegular,
            other => SecretError::Io {
                op: "open",
                source: other.into(),
            },
        })?;
        let mut file = File::from(fd);
        let meta = file.metadata().map_err(io_at("stat"))?;
        if !meta.is_file() {
            return Err(SecretError::NotRegular);
        }
        if meta.mode() & WIDE_BITS != 0 {
            return Err(SecretError::WideMode(meta.mode() & 0o777));
        }
        let mut bytes = Zeroizing::new(Vec::new());
        file.read_to_end(&mut bytes).map_err(io_at("read"))?;
        let stored: Stored = serde_json::from_slice(&bytes).map_err(|_| SecretError::Invalid)?;
        Ok(stored.secret_access_key)
    }

    /// Removes the file of `name` at `version`, if there is one.
    ///
    /// # Errors
    /// A file system call other than "not found" failed.
    pub(super) fn remove(&self, name: &ConnectionName, version: u64) -> Result<(), SecretError> {
        match fs::remove_file(self.path(name, version)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(io_at("remove")(e)),
            _ => Ok(()),
        }
    }

    /// Removes every temp file and every versioned file not in `keep` that is at least
    /// `grace` old, and returns how many. A file this module does not name is left alone.
    ///
    /// # Errors
    /// The directory cannot be listed, or a removal failed.
    pub(super) fn sweep(
        &self,
        keep: &BTreeSet<(String, u64)>,
        grace: Duration,
    ) -> Result<usize, SecretError> {
        let now = SystemTime::now();
        let mut removed = 0;
        for entry in fs::read_dir(&self.dir).map_err(io_at("list the directory"))? {
            let entry = entry.map_err(io_at("list the directory"))?;
            let file_name = entry.file_name();
            let Some(text) = file_name.to_str() else {
                continue;
            };
            let orphan = if text.starts_with(TEMP_PREFIX) {
                true
            } else if let Some(id) = parse_file_name(text) {
                !keep.contains(&id)
            } else {
                false
            };
            // Another frontend child on this host may be between its rename and its row
            // commit: a file is an orphan only once it has sat for the grace.
            let aged = entry
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|at| now.duration_since(at).unwrap_or_default() >= grace);
            if orphan && aged {
                fs::remove_file(entry.path()).map_err(io_at("remove an orphan"))?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

/// The connection and version a file name spells, `{name}.{version:016x}.json`.
fn parse_file_name(text: &str) -> Option<(String, u64)> {
    let stem = text.strip_suffix(".json")?;
    let (name, version) = stem.rsplit_once('.')?;
    name.parse::<ConnectionName>().ok()?;
    if version.len() != 16
        || !version
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    Some((name.to_owned(), u64::from_str_radix(version, 16).ok()?))
}
