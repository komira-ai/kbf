//! `request_id -> update_id`, on disk (FP section 4: "same `request_id` returns the same
//! `update_id`", Q5).
//!
//! The file is replaced whole: written to a temporary name, `fsync`ed, renamed over the
//! old one, and the directory `fsync`ed, so a crash leaves the old map or the new one and
//! never half of one. An entry is written before the 202 is sent (and before the order),
//! so a client whose connection died retries with its `request_id` and gets the update id
//! it would have had.
//!
//! The newest [`KEEP`] entries are kept (FP: "the last 64 pairs"). The file is this
//! module's own (`requests.json` in the proxy state directory); `update.json`, which
//! carries the intent of the swap, is U7's, and U8b may fold this map into it.
//!
//! A file that exists and does not parse is not treated as empty: that would let a retry
//! start a second update. The store reports [`Idempotency::lost`] and the API refuses new
//! updates (`409 state_lost`) until an operator removes the file.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::model::UpdateId;
use crate::CommitId;

/// Entries kept.
pub const KEEP: usize = 64;
const FILE: &str = "requests.json";
const SCHEMA: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub request_id: String,
    pub update_id: UpdateId,
    pub commit: CommitId,
    pub nodes: bool,
    pub rollback: bool,
    /// The order has been written and the update is in motion. An entry that is not
    /// started was accepted and then the process died or the order could not be written.
    pub started: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    schema: u32,
    next_seq: u64,
    entries: Vec<Entry>,
}

#[derive(Debug, thiserror::Error)]
#[error("the request map could not be saved: {0}")]
pub struct SaveError(#[from] std::io::Error);

pub struct Idempotency {
    dir: PathBuf,
    next_seq: u64,
    entries: Vec<Entry>,
    lost: bool,
}

impl Idempotency {
    /// Opens the map in `dir`. A missing file is an empty map; an unreadable one is
    /// [`lost`](Idempotency::lost).
    pub fn open(dir: &Path) -> Idempotency {
        let mut map = Idempotency {
            dir: dir.to_owned(),
            next_seq: 1,
            entries: Vec::new(),
            lost: false,
        };
        match fs::read(dir.join(FILE)) {
            Ok(bytes) => match serde_json::from_slice::<Saved>(&bytes) {
                Ok(f) if f.schema == SCHEMA => {
                    map.next_seq = f.next_seq;
                    map.entries = f.entries;
                }
                _ => map.lost = true,
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => map.lost = true,
        }
        map
    }

    pub fn lost(&self) -> bool {
        self.lost
    }

    pub fn get(&self, request_id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.request_id == request_id)
    }

    /// Records a new request under a fresh update id and makes it durable before it
    /// returns.
    pub fn insert(
        &mut self,
        request_id: &str,
        commit: &CommitId,
        nodes: bool,
        rollback: bool,
    ) -> Result<Entry, SaveError> {
        let entry = Entry {
            request_id: request_id.to_owned(),
            update_id: UpdateId(format!("upd-{}", self.next_seq)),
            commit: commit.clone(),
            nodes,
            rollback,
            started: false,
        };
        self.next_seq += 1;
        self.entries.push(entry.clone());
        if self.entries.len() > KEEP {
            self.entries.remove(0);
        }
        self.save()?;
        Ok(entry)
    }

    /// Marks the entry started, durably.
    pub fn mark_started(&mut self, request_id: &str) -> Result<(), SaveError> {
        if let Some(e) = self.entries.iter_mut().find(|e| e.request_id == request_id) {
            e.started = true;
            self.save()?;
        }
        Ok(())
    }

    fn save(&self) -> Result<(), SaveError> {
        let file = Saved {
            schema: SCHEMA,
            next_seq: self.next_seq,
            entries: self.entries.clone(),
        };
        let mut bytes = serde_json::to_vec(&file).map_err(std::io::Error::other)?;
        bytes.push(b'\n');
        write_atomic(&self.dir, FILE, &bytes)?;
        Ok(())
    }
}

/// Temp, write, `fsync`, rename, `fsync` the directory.
pub fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = dir.join(format!(".{name}.tmp"));
    let mut f = File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, dir.join(name))?;
    File::open(dir)?.sync_all()
}
