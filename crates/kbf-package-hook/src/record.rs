//! The hook's durable record: what this update did to the unit. Every step reads it and decides
//! from it, never from the order the steps were called in.
//!
//! One file, `record.json`, in the record directory (`state_dir`). It is replaced whole (written
//! beside, synced, renamed over, the directory synced), so a step killed at any point leaves the
//! old record or the new one. The record is written before the act it stands for (`begun`
//! before the unit is stopped), so that a step run again finds it and looks at the real state
//! before it acts a second time. It holds no secret and no time.
//!
//! The directory is locked, exclusively, for the whole step ([`Store::lock`]).

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The layout this binary writes; a record with a higher number is not touched.
pub const SCHEMA: u32 = 1;

/// The record's file in the record directory.
pub const FILE: &str = "record.json";

/// How far one update got.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Record {
    pub schema: u32,
    /// The version this update installs, and the digest of its artifact (empty if none was given).
    pub target: String,
    pub digest: String,
    /// There was no previous release: this update is the install.
    pub first_install: bool,
    /// The unit was running before this update stopped it. Read once, before the stop, and kept.
    pub was_active: bool,
    /// `prepare` finished: the unit is stopped.
    pub prepared: bool,
    /// The unit was started by `cutover`.
    pub started: bool,
    /// `verify` saw the target serve.
    pub verified: bool,
    /// The update was undone.
    pub rolled_back: bool,
}

impl Default for Record {
    fn default() -> Record {
        Record {
            schema: SCHEMA,
            target: String::new(),
            digest: String::new(),
            first_install: false,
            was_active: false,
            prepared: false,
            started: false,
            verified: false,
            rolled_back: false,
        }
    }
}

impl Record {
    /// The record of a new update.
    pub fn new(target: &str, digest: &str, first_install: bool) -> Record {
        Record {
            target: target.to_owned(),
            digest: digest.to_owned(),
            first_install,
            ..Record::default()
        }
    }

    /// Whether this is the record of the update `target` with `digest` names.
    pub fn is_of(&self, target: &str, digest: &str) -> bool {
        self.target == target && self.digest == digest
    }
}

/// Why the record could not be read or kept.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("the record directory cannot be locked: another copy of this step is running")]
    Busy,
    #[error("{what} {path}: {source}")]
    Io {
        what: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("the record {path} is not one this hook wrote: {why}")]
    Corrupt { path: PathBuf, why: String },
    #[error("the record {path} has schema {found}, newer than this hook's {SCHEMA}; not touched")]
    Newer { path: PathBuf, found: u32 },
}

/// The record directory. `before_write` is called before every change of the record, so that
/// a test can stop the step there (`update_hook::conformance::gate::point`); a real run has none.
pub struct Store {
    dir: PathBuf,
    before_write: Option<fn()>,
}

impl Store {
    pub fn new(dir: PathBuf, before_write: Option<fn()>) -> Store {
        Store { dir, before_write }
    }

    fn io(&self, what: &'static str, source: std::io::Error) -> StoreError {
        StoreError::Io {
            what,
            path: self.dir.clone(),
            source,
        }
    }

    /// Makes the directory if it is not there, readable by its owner alone.
    ///
    /// # Errors
    /// The directory cannot be made.
    pub fn ensure(&self) -> Result<(), StoreError> {
        fs::create_dir_all(&self.dir).map_err(|e| self.io("cannot make the record directory", e))
    }

    /// Takes the exclusive lock on the directory for as long as the returned file lives.
    ///
    /// # Errors
    /// [`StoreError::Busy`] when another process holds it; an `Io` when the directory cannot be
    /// opened.
    pub fn lock(&self) -> Result<File, StoreError> {
        let dir =
            File::open(&self.dir).map_err(|e| self.io("cannot open the record directory", e))?;
        dir.try_lock().map(|()| dir).map_err(|_| StoreError::Busy)
    }

    /// The record, if there is one.
    ///
    /// # Errors
    /// The file cannot be read, is not a record, or is newer than this hook.
    pub fn load(&self) -> Result<Option<Record>, StoreError> {
        let path = self.dir.join(FILE);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(self.io("cannot read the record in", e)),
        };
        let record: Record = serde_json::from_str(&text).map_err(|e| StoreError::Corrupt {
            path: path.clone(),
            why: e.to_string(),
        })?;
        if record.schema > SCHEMA {
            return Err(StoreError::Newer {
                path,
                found: record.schema,
            });
        }
        Ok(Some(record))
    }

    fn gate(&self) {
        if let Some(gate) = self.before_write {
            gate();
        }
    }

    /// Replaces the record: written beside, synced, renamed over, the directory synced.
    ///
    /// # Errors
    /// The write failed; the old record is then still the record.
    pub fn save(&self, record: &Record) -> Result<(), StoreError> {
        self.gate();
        let path = self.dir.join(FILE);
        let tmp = self.dir.join(format!("{FILE}.tmp"));
        let written = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)
            .and_then(|mut file| {
                serde_json::to_writer_pretty(&mut file, record)
                    .map_err(std::io::Error::from)
                    .and_then(|()| file.write_all(b"\n"))
                    .and_then(|()| file.sync_all())
            })
            .and_then(|()| fs::rename(&tmp, &path))
            .and_then(|()| self.sync_dir());
        written.map_err(|e| self.io("cannot write the record in", e))
    }

    /// Removes the record; none is no error.
    ///
    /// # Errors
    /// The file could not be removed.
    pub fn remove(&self) -> Result<(), StoreError> {
        self.gate();
        match fs::remove_file(self.dir.join(FILE)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(self.io("cannot remove the record in", e))
            }
            _ => self
                .sync_dir()
                .map_err(|e| self.io("cannot sync the record directory", e)),
        }
    }

    fn sync_dir(&self) -> std::io::Result<()> {
        File::open(&self.dir).and_then(|dir| dir.sync_all())
    }
}
