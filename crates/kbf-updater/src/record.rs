//! `roll.json`: the durable record of the current roll, in the updater's state directory.
//!
//! Written by temp file (mode 0600), `sync_all`, rename and a directory fsync, before every
//! phase change, so a reader sees the old record or the new one and never a torn one. The
//! pattern is the proxy's `update.json`, copied rather than shared: kbf-installer is the
//! crate this package exists to outlive.
//!
//! A missing file is `Ok(None)`: no roll has run. A file that cannot be read, does not parse,
//! carries a format this build does not know, or contradicts itself is an error, and the
//! updater refuses to start on it (fail closed): a guess about where a roll stood is a guess
//! about which host is stopped.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::api::{Hosts, Phase, Roll, RollRequest, RollState};

const NAME: &str = "roll.json";
const TEMP: &str = ".roll.json.tmp";
const LOCK: &str = "lock";

/// The format written and the only one read.
pub const FORMAT: u32 = 1;

/// The id of the roll with this sequence number.
pub fn roll_id(seq: u64) -> String {
    format!("roll-{seq}")
}

/// What is on disk: the roll as the poller sees it, plus what the updater needs to resume it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub format: u32,
    /// Counts rolls; the id is made from it.
    pub seq: u64,
    pub request: RollRequest,
    pub roll: Roll,
    /// How many times the updater restarted while this roll was running.
    pub resumes: u32,
    /// An operator asked for an abort; it takes effect between hosts.
    pub abort_requested: bool,
}

impl Record {
    /// The record contradicts itself: for an explicit host list, the roll's hosts are not the
    /// request's aliases (the same ones, in the same order, with some moved to the front for
    /// the frontends; [`crate::Updater::open`] holds the exact plan to the inventory);
    /// a host is in flight while an earlier one is not done, a later one has begun, or the
    /// state says more than the hosts do. A hand-edited or half-merged file lands here and
    /// not in a roll that touches the wrong host.
    pub fn check(&self) -> Result<(), String> {
        if self.format != FORMAT {
            return Err(format!("format {} is not {FORMAT}", self.format));
        }
        if self.roll.id != roll_id(self.seq) {
            return Err(format!(
                "roll id {} is not the id of roll {}",
                self.roll.id, self.seq
            ));
        }
        let hosts = &self.roll.hosts;
        if hosts.is_empty() {
            return Err("the roll has no hosts".to_owned());
        }
        if let Hosts::Aliases(asked) = &self.request.hosts {
            let have: Vec<&str> = hosts.iter().map(|h| h.alias.as_str()).collect();
            if !is_plan_of(&have, asked) {
                return Err(format!(
                    "the roll's hosts {have:?} are not the request's {asked:?}"
                ));
            }
        }
        // Done*, then at most one host in flight (or failed), then Pending*.
        let mut open = false;
        for h in hosts {
            if open && h.phase != Phase::Pending {
                return Err(format!(
                    "host {} is {:?} after an unfinished host",
                    h.alias, h.phase
                ));
            }
            open |= h.phase != Phase::Done;
        }
        let failure = self.roll.failure.is_some();
        let any_failed = hosts.iter().any(|h| h.phase == Phase::Failed);
        match self.roll.state {
            RollState::Done if open => Err("the roll is done and a host is not".to_owned()),
            RollState::Failed if !(failure && any_failed) => {
                Err("the roll is failed without a failure and a failed host".to_owned())
            }
            s if s != RollState::Failed && (failure || any_failed) => {
                Err("a failure is recorded and the roll is not failed".to_owned())
            }
            _ => Ok(()),
        }
    }
}

/// `have` is `asked` as the plan makes it: the same aliases, with the frontends (one or, for
/// pool members, several) moved to the front and everything else in its order. So some first
/// `k` of `have` are in `asked`'s order, and the rest follow in `asked`'s order.
fn is_plan_of(have: &[&str], asked: &[String]) -> bool {
    if have.len() != asked.len() {
        return false;
    }
    (0..=have.len()).any(|k| {
        let front = &have[..k];
        let first = asked.iter().filter(|a| front.contains(&a.as_str()));
        let rest = asked.iter().filter(|a| !front.contains(&a.as_str()));
        let planned = first.chain(rest).map(String::as_str);
        have.iter().copied().eq(planned)
    })
}

/// Why the record could not be used.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("roll.json cannot be read: {0}")]
    Io(#[from] std::io::Error),
    #[error("roll.json is corrupt: {0}")]
    Corrupt(#[from] serde_json::Error),
    #[error("roll.json contradicts itself: {0}")]
    Inconsistent(String),
    #[error("the state directory cannot be locked: another kbf-updater holds it")]
    Busy,
}

/// The two syncs that make a record durable. A test passes one that looks at the directory
/// when each is called, or fails after a count, so that dropping a sync, moving it before the
/// rename or losing the process between two writes is visible (a crash cannot be staged in a
/// test, a call order can).
pub(crate) trait Durable: Send + Sync + std::fmt::Debug {
    /// Flush the temp file's data, before the rename.
    fn file(&self, file: &File) -> std::io::Result<()>;
    /// Flush the directory entry, after the rename.
    fn dir(&self, dir: &Path) -> std::io::Result<()>;
}

#[derive(Debug)]
pub(crate) struct Fsync;

impl Durable for Fsync {
    fn file(&self, file: &File) -> std::io::Result<()> {
        file.sync_all()
    }

    fn dir(&self, dir: &Path) -> std::io::Result<()> {
        File::open(dir)?.sync_all()
    }
}

/// Reads and writes `roll.json` under a directory.
///
/// [`RollStore::open`] takes an exclusive `flock` on `lock` in the directory and holds it
/// until the store is dropped, so a second updater process over the same directory is
/// refused ([`StoreError::Busy`]) and not left to race the first on `roll.json`.
#[derive(Debug)]
pub struct RollStore {
    dir: PathBuf,
    sync: Box<dyn Durable>,
    /// Held for its lock; never read.
    _lock: Option<File>,
}

impl RollStore {
    /// The store in `dir` (which must exist), with the real fsyncs, and the directory locked.
    pub fn open(dir: &Path) -> Result<RollStore, StoreError> {
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(dir.join(LOCK))?;
        lock.try_lock().map_err(|_| StoreError::Busy)?;
        Ok(RollStore {
            dir: dir.to_owned(),
            sync: Box::new(Fsync),
            _lock: Some(lock),
        })
    }

    /// A store without the directory lock and with `sync` for its syncs: the test seam.
    #[cfg(test)]
    pub(crate) fn with_sync(dir: &Path, sync: Box<dyn Durable>) -> RollStore {
        RollStore {
            dir: dir.to_owned(),
            sync,
            _lock: None,
        }
    }

    pub fn load(&self) -> Result<Option<Record>, StoreError> {
        let bytes = match fs::read(self.dir.join(NAME)) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let record: Record = serde_json::from_slice(&bytes)?;
        record.check().map_err(StoreError::Inconsistent)?;
        Ok(Some(record))
    }

    pub fn save(&self, record: &Record) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(record).map_err(std::io::Error::other)?;
        let temp = self.dir.join(TEMP);
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(&bytes)?;
        self.sync.file(&file)?;
        fs::rename(&temp, self.dir.join(NAME))?;
        self.sync.dir(&self.dir)
    }
}
