//! What the installer knows about the frontend between passes: which instance serves
//! (`serving_instance`) and the one update in flight (`frontend_pending`).
//!
//! Both live in [`FrontendFile`], `frontend.json` beside `state.json`, root-owned. They are
//! the two fields the host state of the next schema will carry (`serving_instance` and
//! `frontend_pending`, types as below); until then every read and write goes through
//! [`load`] and [`save`], so moving them into `state.json` changes those two functions and
//! nothing else.
//!
//! The pending record is written before anything it describes happens (a link moved, a unit
//! started), and every step after it is decided from the record and what systemd and the
//! instances' `/readyz` say now, never from what this process did before it was killed. So a
//! pass that starts from the record resumes at the right branch whatever the last pass got
//! to.

use std::io;

use kbf_host::{Exec, HostPath, Perms, Step};
use serde::{Deserialize, Serialize};

use crate::CommitId;
use crate::layout::Layout;

/// One of the two instances of `kbf-frontend@.service`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Instance {
    A,
    B,
}

impl Instance {
    pub const ALL: [Instance; 2] = [Instance::A, Instance::B];

    /// The word in the unit name and in `slots/<instance>`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Instance::A => "a",
            Instance::B => "b",
        }
    }

    #[must_use]
    pub fn other(self) -> Instance {
        match self {
            Instance::A => Instance::B,
            Instance::B => Instance::A,
        }
    }

    /// The unit's name, from this table and never from a record.
    #[must_use]
    pub fn unit(self) -> String {
        format!("kbf-frontend@{}.service", self.name())
    }
}

/// Where the update is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum Phase {
    /// The new instance was (or is being) started; waiting for it to be ready, for the old
    /// one to have stood down, and for that to hold for enough looks in a row.
    Confirming { looks: u32 },
    /// Decided to keep it: `current`, `previous` and `serving_instance` are being moved.
    Flipping,
    /// Decided to go back. The new instance is stopped; the old one is brought back if it
    /// stood down; the slot is put back last. `deadline` bounds the stop and the start of
    /// the old instance. `old_started` is written before the installer starts the old
    /// instance again, so an instance that is `active` but not yet answering `/readyz` after
    /// that is waited for, not taken for one that stood down and stopped a second time.
    Reverting { deadline: u64, old_started: bool },
    /// The old instance would not stop or come back in time. Nothing is killed; an
    /// operator looks. The record stays until one does.
    NeedsOperator { reason: String },
}

/// The one frontend update in flight.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendPending {
    /// The order's (`epoch`, `order_id`), so a record read twice is one update, and so
    /// `status.json` can name the order an outcome belongs to.
    pub epoch: u64,
    pub order_id: u64,
    pub commit: CommitId,
    /// The instance the update starts: the one that was idle.
    pub idle: Instance,
    /// The instance that served when the update began.
    pub old: Instance,
    /// What `slots/<idle>` named before the update (the link's text); `None` when there
    /// was no link.
    pub prior_slot: Option<String>,
    /// The end of the confirm window, in the clock the passes are given.
    pub deadline_s: u64,
    pub phase: Phase,
    /// Why it is being reverted, once it is.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub why: String,
}

/// How the last frontend order ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum End {
    /// The new instance serves (also: the serving instance already ran that release).
    Confirmed,
    /// The update was undone.
    Reverted,
    /// The order was refused before anything moved.
    Refused,
}

/// The last frontend order that ended, written in the same save that ends its pending
/// record, so `status.json` can name the order an outcome belongs to even after a reboot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendEnd {
    pub epoch: u64,
    pub order_id: u64,
    pub commit: CommitId,
    pub end: End,
    /// Why it was undone or refused; empty otherwise.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub why: String,
}

/// `frontend.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrontendFile {
    /// The instance that last served by the installer's own record; a hint, since systemd
    /// is the fact (an update refuses when the two disagree).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serving_instance: Option<Instance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frontend_pending: Option<FrontendPending>,
    /// How the newest frontend order ended (a pending record is the newer word while one exists).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<FrontendEnd>,
}

/// Why `frontend.json` could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum FileError {
    #[error("cannot read frontend.json: {0}")]
    Read(io::Error),
    #[error("frontend.json is not a frontend record: {0}")]
    Parse(serde_json::Error),
    #[error("cannot write frontend.json: {0}")]
    Write(io::Error),
}

fn path(host: &Layout) -> HostPath {
    HostPath::new(&format!("{}/frontend.json", host.state_dir().as_str()))
        .expect("the state directory and a plain name")
}

/// The record; empty when there is none.
pub fn load(host: &Layout) -> Result<FrontendFile, FileError> {
    match std::fs::read(host.join(&path(host))) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(FileError::Parse),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(FrontendFile::default()),
        Err(e) => Err(FileError::Read(e)),
    }
}

/// Replace the record, atomically (the executor's write is a rename).
pub fn save<E: Exec>(exec: &mut E, host: &Layout, file: &FrontendFile) -> Result<(), FileError> {
    let mut bytes = serde_json::to_vec_pretty(file).expect("plain fields serialize");
    bytes.push(b'\n');
    exec.apply(&Step::WriteFile {
        path: path(host),
        perms: Perms::new(0o600),
        owner: None,
        bytes,
    })
    .map_err(FileError::Write)
}

#[cfg(test)]
#[path = "pending_tests.rs"]
mod tests;
