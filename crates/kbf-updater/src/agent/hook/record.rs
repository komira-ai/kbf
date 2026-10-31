//! The hook's own durable record: what this update placed, changed and finished
//! (`docs/design/host-updates.md`, section 3.2). Every step reads it and decides from it, and
//! none decides from the order the updater called the steps in.
//!
//! One file, `record.json`, in the record directory (`--record-dir`, the declaration's
//! `state_dir`). It is replaced whole (written beside, synced, renamed over, the directory
//! synced), so a step killed at any point leaves the old record or the new one. A mark is
//! written before the act it stands for (`drain_begun` before the drain, `switch_begun` before
//! the switch, `confirm_begun` before the confirm), so that a step that is run again finds the
//! mark and looks at the real state before it acts a second time.
//!
//! The record holds no secret and no time: a step run again that changes nothing writes
//! nothing, and the same update always leaves the same record.
//!
//! The directory is locked, exclusively, for the whole step ([`Store::lock`]): a second copy of
//! a step refuses and changes nothing.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::ports::Placed;

/// The record layout this binary writes. A record with a higher number was written by a newer
/// hook and is not touched.
pub const SCHEMA: u32 = 1;

/// The record's file in the record directory.
pub const FILE: &str = "record.json";

/// Whether the node was in placement before this update drained it: [`Placed`], as the record
/// spells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Was {
    /// The node was serving, or held a cordon of this updater's own from an earlier attempt.
    In,
    /// An operator had taken it out of placement: this update never gives it back.
    Out,
}

impl From<Placed> for Was {
    fn from(placed: Placed) -> Was {
        match placed {
            Placed::In => Was::In,
            Placed::Out => Was::Out,
        }
    }
}

/// How far one update got. Every field but the identity starts false or absent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Record {
    pub schema: u32,
    /// The version this update installs.
    pub target: String,
    /// The artifact digest the updater verified for it: with the target, the update's identity.
    pub digest: String,
    /// The host runs the target already: no step has anything to do.
    pub current: bool,
    /// Whether the node was in placement before this update's own drain. Written before the
    /// drain, read once, never read again: it is what decides, in `rollback` and `cleanup`,
    /// whether the node is given back.
    pub placement: Option<Was>,
    /// The drain was begun (written before it). Cleared again when the drain was refused.
    pub drain_begun: bool,
    /// `prepare` finished: the node holds no lease and nothing was installed over a lease.
    pub prepared: bool,
    /// The switch was begun (written before it).
    pub switch_begun: bool,
    /// The helper's deadline for the trial, Unix seconds; present once the switch is done.
    pub deadline_unix: Option<u64>,
    /// `verify` saw the target healthy.
    pub verified: bool,
    /// The confirm was begun (written before it). Cleared again when the helper refused it.
    pub confirm_begun: bool,
    /// The helper has confirmed the target: the commit.
    pub confirmed: bool,
    /// The node was given back, or was never ours to give back.
    pub given_back: bool,
    /// The update was undone: the old release runs and the node was given back if it was ours.
    pub rolled_back: bool,
}

impl Default for Record {
    fn default() -> Record {
        Record {
            schema: SCHEMA,
            target: String::new(),
            digest: String::new(),
            current: false,
            placement: None,
            drain_begun: false,
            prepared: false,
            switch_begun: false,
            deadline_unix: None,
            verified: false,
            confirm_begun: false,
            confirmed: false,
            given_back: false,
            rolled_back: false,
        }
    }
}

impl Record {
    /// The record of a new update.
    pub fn new(target: &str, digest: &str) -> Record {
        Record {
            target: target.to_owned(),
            digest: digest.to_owned(),
            ..Record::default()
        }
    }

    /// Whether this is the record of the update `target` with `digest` names.
    pub fn is_of(&self, target: &str, digest: &str) -> bool {
        self.target == target && self.digest == digest
    }

    /// The host runs the target already: there is nothing to prepare, switch or confirm, and
    /// every step after this one succeeds without a call.
    pub fn made_current(&mut self) {
        self.current = true;
        self.prepared = true;
    }

    /// The update is over, one way or the other: a new `prepare` for the same release starts a
    /// new one.
    pub fn is_over(&self) -> bool {
        self.rolled_back || (self.confirmed && self.given_back)
    }
}

/// Why the record could not be read or kept.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Another copy of a step holds the directory.
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

/// What is called before every write of the record, in the hook's process, so that a test can
/// stop the step at each change (`update_hook::conformance::gate::point`). There is none in a
/// real run.
pub type Gate = Arc<dyn Fn() + Send + Sync>;

/// The record directory.
pub struct Store {
    dir: PathBuf,
    before_write: Option<Gate>,
}

impl Store {
    pub fn new(dir: PathBuf, before_write: Option<Gate>) -> Store {
        Store { dir, before_write }
    }

    fn io(&self, what: &'static str, source: std::io::Error) -> StoreError {
        StoreError::Io {
            what,
            path: self.dir.clone(),
            source,
        }
    }

    /// Takes the exclusive lock on the directory for as long as the returned file lives.
    ///
    /// # Errors
    /// [`StoreError::Busy`] when the lock cannot be taken (another process holds it), or the
    /// directory cannot be opened.
    pub fn lock(&self) -> Result<File, StoreError> {
        let dir =
            File::open(&self.dir).map_err(|e| self.io("cannot open the record directory", e))?;
        // Any refusal of the lock is a refusal of the step: the record is never shared.
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

    /// Replaces the record: written beside, synced, renamed over, the directory synced.
    ///
    /// # Errors
    /// The write failed; the old record is then still the record.
    pub fn save(&self, record: &Record) -> Result<(), StoreError> {
        if let Some(gate) = &self.before_write {
            gate();
        }
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
            .and_then(|()| File::open(&self.dir).and_then(|dir| dir.sync_all()));
        written.map_err(|e| self.io("cannot write the record in", e))
    }
}
