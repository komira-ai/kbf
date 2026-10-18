//! The old layout, read by descriptor: the deploy state (`state.json`, schema 1), what the
//! old helper remembered (`helper.json`), and the releases the `current` and `previous`
//! links name.

use std::os::fd::OwnedFd;

use rustix::io::Errno;

use super::error::MigrateError;
use super::roots::Roots;
use super::tree::{self, Rules};
use crate::CommitId;
use crate::helper::memory::Memory;
use crate::layout::Layout;
use crate::state::{self, State};

const FILE_CAP: u64 = 1 << 20;

/// What the old tools recorded.
#[derive(Debug)]
pub struct Old {
    pub state: State,
    pub memory: Memory,
}

/// Read the old state; `None` when the old layout has neither file (not an old host).
pub fn read(layout: &Layout, roots: &Roots, rules: &Rules) -> Result<Option<Old>, MigrateError> {
    let path = layout.join(&roots.old_state);
    let dir = match tree::open_root(&path) {
        Ok(dir) => dir,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(MigrateError::io(roots.old_state.to_string())(e)),
    };
    let state = tree::read_file(rules, &dir, "state.json", FILE_CAP)?;
    let memory = tree::read_file(rules, &dir, "helper.json", FILE_CAP)?;
    if state.is_none() && memory.is_none() {
        return Ok(None);
    }
    let state = match state {
        Some(bytes) => state::parse(&path.join("state.json"), &bytes)?,
        None => State::default(),
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
        let target = match rustix::fs::readlinkat(&self.tree, link, Vec::new()) {
            Ok(target) => target.to_string_lossy().into_owned(),
            Err(Errno::NOENT) => return Ok(None),
            Err(e) => return Err(MigrateError::io(link)(e.into())),
        };
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
