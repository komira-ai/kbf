//! The old layout, read by descriptor: the deploy state (`state.json`, schema 1), what the
//! old helper remembered (`helper.json`), and the releases the `current` and `previous`
//! links name.

use std::fs::{File, TryLockError};
use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use super::error::MigrateError;
use super::roots::Roots;
use super::tree::{self, Rules};
use crate::CommitId;
use crate::helper::memory::Memory;
use crate::layout::Layout;
use crate::state::StateError;
use serde::Deserialize;

const FILE_CAP: u64 = 1 << 20;

/// The `schema` number of the deploy kit's `state.json`, the only one this reads. The
/// installer's own is [`crate::state::SCHEMA`] (2), and its type is [`crate::state::State`]:
/// this one is read here and nowhere else.
const OLD_SCHEMA: u64 = 1;

/// `state.json` as the deploy kit's tools wrote it (schema 1). Every key it had is named, so
/// a file with another one is refused rather than half read. Only the swap on trial is
/// looked into (its commit is named in the refusal); the rest of it is not carried over.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OldState {
    schema: u64,
    pub current: Option<CommitId>,
    pub previous: Option<CommitId>,
    pub staged: Option<CommitId>,
    pub pending_confirm: Option<OldPending>,
    /// Never read by anything: named so that the file parses.
    #[allow(dead_code)]
    order_floor: u64,
}

/// A swap of the old tools on trial. It stops the migration, so only the release it names
/// is read; its other keys are whatever the old tool wrote.
#[derive(Debug, Deserialize)]
pub struct OldPending {
    pub commit: CommitId,
    #[serde(flatten)]
    _rest: serde_json::Map<String, serde_json::Value>,
}

/// The old state's bytes: schema 1 or refused.
fn parse_state(bytes: &[u8]) -> Result<OldState, MigrateError> {
    let corrupt = |why: String| MigrateError::Corrupt {
        file: "state.json",
        why,
    };
    let state: OldState = serde_json::from_slice(bytes).map_err(|e| corrupt(e.to_string()))?;
    if state.schema != OLD_SCHEMA {
        return Err(corrupt(format!(
            "schema {}, the old layout's is {OLD_SCHEMA}",
            state.schema
        )));
    }
    Ok(state)
}

/// What the old tools recorded.
#[derive(Debug)]
pub struct Old {
    pub state: OldState,
    pub memory: Memory,
}

/// Read the old state; `None` when the old layout has neither file (not an old host).
pub fn read(layout: &Layout, roots: &Roots, rules: &Rules) -> Result<Option<Old>, MigrateError> {
    let path = layout.join(&roots.old_state);
    let opened = tree::absent(tree::open_root(&path));
    let Some(dir) = opened.map_err(MigrateError::io(roots.old_state.to_string()))? else {
        return Ok(None);
    };
    let state = tree::read_file(rules, &dir, "state.json", FILE_CAP)?;
    let memory = tree::read_file(rules, &dir, "helper.json", FILE_CAP)?;
    if state.is_none() && memory.is_none() {
        return Ok(None);
    }
    let state = match state {
        Some(bytes) => parse_state(&bytes)?,
        // Only the helper's memory is there: an old host with nothing deployed.
        None => OldState {
            schema: OLD_SCHEMA,
            current: None,
            previous: None,
            staged: None,
            pending_confirm: None,
            order_floor: 0,
        },
    };
    let memory = match memory {
        Some(bytes) => parse_memory(&bytes)?,
        None => Memory::default(),
    };
    Ok(Some(Old { state, memory }))
}

/// `helper.json` as the deployed helpers wrote it. The helper of `feature/pull-updates` also
/// kept `handled_entry`, the newest inbox entry it looked at; there is no inbox now and
/// nothing reads it, so it is dropped before the strict parse. Every other key this
/// build does not know is still refused.
fn parse_memory(bytes: &[u8]) -> Result<Memory, MigrateError> {
    let corrupt = |why: String| MigrateError::Corrupt {
        file: "helper.json",
        why,
    };
    let mut object: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(bytes).map_err(|e| corrupt(e.to_string()))?;
    object.remove("handled_entry");
    serde_json::from_value(serde_json::Value::Object(object)).map_err(|e| corrupt(e.to_string()))
}

/// The old tree, open: the directory of `releases/`.
pub struct OldTree {
    tree: OwnedFd,
    releases: OwnedFd,
}

impl OldTree {
    pub fn open(layout: &Layout, roots: &Roots) -> Result<OldTree, MigrateError> {
        let path = layout.join(&roots.old_tree);
        let tree = tree::open_root(&path).map_err(MigrateError::io(roots.old_tree.to_string()))?;
        let releases = tree::open_dir(&tree, "releases").map_err(MigrateError::io("releases"))?;
        Ok(OldTree { tree, releases })
    }

    /// The release a link (`current` or `previous`) names, which must be exactly
    /// `releases/<40 hex>`; `None` when there is no such link.
    pub fn linked(&self, link: &'static str) -> Result<Option<CommitId>, MigrateError> {
        let read = tree::absent(rustix::fs::readlinkat(&self.tree, link, Vec::new()));
        let Some(target) = read.map_err(MigrateError::io(link))? else {
            return Ok(None);
        };
        let target = target.to_string_lossy().into_owned();
        target
            .strip_prefix("releases/")
            .and_then(|name| CommitId::new(name).ok())
            .map(Some)
            .ok_or(MigrateError::BadLink { link, target })
    }

    /// The release directory `name`, opened without following a link.
    pub fn release(&self, name: &CommitId) -> Result<OwnedFd, MigrateError> {
        tree::open_dir(&self.releases, name.as_str()).map_err(MigrateError::io(name.as_str()))
    }
}

/// The lock the old tools changed their state under, held: no verb of theirs is mid-way
/// while the migration reads their state and tree. Dropping it lets go.
///
/// It is `flock(2)` on the file the deploy kit's own `Locked` takes it on (`File::lock`),
/// so the kit's verbs and this wait for each other. That the builds on a host do the same
/// is a check on the host (docs/deploy/kbf-installer-migrate.md).
pub struct OldLock {
    file: File,
}

impl Drop for OldLock {
    fn drop(&mut self) {
        // As the installer's own lock does ([`crate::state::Locked`]): a copy of the
        // descriptor in a child between fork and exec would keep the lock past the close.
        let _ = self.file.unlock();
    }
}

impl OldLock {
    /// Take the lock (made, 0600, when missing), waiting at most `wait` while another holds
    /// it: [`StateError::Busy`] (exit 4) after that, for the old tools may hold it a long
    /// while and the installer's lock is waited for next.
    pub fn take(layout: &Layout, roots: &Roots, wait: Duration) -> Result<OldLock, MigrateError> {
        use std::os::unix::fs::OpenOptionsExt;
        let path = layout.join(&roots.old_lock);
        let what = roots.old_lock.to_string();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .map_err(MigrateError::io(what.as_str()))?;
        let end = Instant::now() + wait;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(OldLock { file }),
                Err(TryLockError::WouldBlock) if Instant::now() < end => {
                    std::thread::sleep(POLL);
                }
                Err(TryLockError::WouldBlock) => {
                    return Err(StateError::Busy { path }.into());
                }
                Err(TryLockError::Error(e)) => return Err(MigrateError::io(what)(e)),
            }
        }
    }
}

/// How often a lock held by someone else is tried again.
const POLL: Duration = Duration::from_millis(20);
