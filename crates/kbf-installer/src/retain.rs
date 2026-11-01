//! Which releases a host keeps, and the one file that says what must stay besides the links.
//!
//! A host keeps, in `releases/`, the release that runs (`current`), the one before it
//! (`previous`), every pinned release, the coordinator's `last_good`, and the newest
//! [`DEFAULT_KEEP`] others (never fewer than [`MIN_KEEP`]), so going back to any of them needs
//! no download. Releases are removed only after a swap is confirmed, never while one is on
//! trial ([`crate::release::gc`]).
//!
//! `retain.json`, beside the state file, records what the links and the state do not:
//!
//! ```json
//! {"pinned": ["<commit>", ...], "last_good": "<commit>", "versions": {"<commit>": "<version>"}}
//! ```
//!
//! It is a file of its own, not a field of `state.json`, because the state is read strictly
//! (`deny_unknown_fields`): an older release that reads a state with a new field refuses it,
//! and going back to an older release is what this file is for. Every key is optional and an
//! unknown key is ignored, so a release before or after this one reads what the other wrote.
//! A file that is there and cannot be parsed removes nothing: it may hold a pin.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use super::CommitId;
use super::layout::Layout;

/// How many releases a host keeps besides current, previous, the pinned ones and `last_good`.
pub const DEFAULT_KEEP: usize = 4;

/// The fewest of those a host ever keeps, whatever a flag says.
pub const MIN_KEEP: usize = 2;

/// The name of the file, in the host's state directory.
pub const FILE: &str = "retain.json";

/// The most bytes of the file that are read.
const MAX_BYTES: u64 = 1024 * 1024;

/// `keep`, raised to [`MIN_KEEP`].
#[must_use]
pub fn at_least(keep: usize) -> usize {
    keep.max(MIN_KEEP)
}

/// A count of releases from a flag: [`MIN_KEEP`] or more.
///
/// # Errors
/// A text that is not a number, or a number below [`MIN_KEEP`].
pub fn parse_keep(text: &str) -> Result<usize, String> {
    let n: usize = text
        .parse()
        .map_err(|_| format!("{text:?} is not a count of releases"))?;
    if n < MIN_KEEP {
        return Err(format!(
            "a host keeps at least {MIN_KEEP} releases besides the ones in use, not {n}"
        ));
    }
    Ok(n)
}

/// Why `retain.json` cannot be read or written.
#[derive(Debug, thiserror::Error)]
pub enum RetainError {
    #[error("cannot read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("{path} cannot be parsed, so nothing is removed (it may hold a pin): {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("cannot write {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
    #[error("cannot lock {path}: {source}")]
    Lock { path: PathBuf, source: io::Error },
}

/// The lock of `retain.json`: an exclusive `flock(2)` on `retain.lock` beside it. Every change
/// of the file is made holding it, and a prune holds it from reading the file to the last
/// removal, so a pin or a `last_good` written meanwhile is neither lost nor pruned. It is the
/// last lock taken: nothing takes another lock while it holds this one.
#[derive(Debug)]
pub struct RetainLock {
    /// None when the directory of the file is not there: then neither is the file, and no
    /// writer can make it there.
    _lock: Option<fs::File>,
    path: PathBuf,
}

impl RetainLock {
    /// Wait for the lock of the file at `path`.
    ///
    /// # Errors
    /// The lock file cannot be opened or locked.
    pub fn acquire(path: &Path) -> Result<Self, RetainError> {
        let lock_path = path.with_extension("lock");
        let fail = |source| RetainError::Lock {
            path: lock_path.clone(),
            source,
        };
        let opened = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path);
        let lock = match opened {
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            opened => {
                let lock = opened.map_err(fail)?;
                lock.lock().map_err(fail)?;
                Some(lock)
            }
        };
        Ok(Self {
            _lock: lock,
            path: path.to_owned(),
        })
    }

    /// The file, as it is now.
    ///
    /// # Errors
    /// The file cannot be read, or parsed.
    pub fn read(&self) -> Result<Retained, RetainError> {
        Retained::read(&self.path)
    }

    /// Replace the file by `retained`.
    ///
    /// # Errors
    /// The file cannot be written.
    pub fn write(&self, retained: &Retained) -> Result<(), RetainError> {
        retained.write(&self.path)
    }
}

/// What `retain.json` holds.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retained {
    /// Releases kept whatever their age: an operator's pin, written into this file.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub pinned: BTreeSet<CommitId>,
    /// The coordinator's last good release, as the last order named it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_good: Option<CommitId>,
    /// The version text each release was installed as (a release's files hold only its
    /// commit).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub versions: BTreeMap<CommitId, String>,
}

impl Retained {
    /// The file at `path`; nothing when there is none.
    ///
    /// # Errors
    /// A file that cannot be read, or parsed.
    pub fn read(path: &Path) -> Result<Self, RetainError> {
        let text = match fs::File::open(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(RetainError::Read {
                    path: path.to_owned(),
                    source,
                });
            }
            Ok(file) => {
                let mut text = String::new();
                io::Read::read_to_string(&mut io::Read::take(file, MAX_BYTES), &mut text).map_err(
                    |source| RetainError::Read {
                        path: path.to_owned(),
                        source,
                    },
                )?;
                text
            }
        };
        serde_json::from_str(&text).map_err(|source| RetainError::Parse {
            path: path.to_owned(),
            source,
        })
    }

    /// Replace the file at `path` by this, whole: written beside it and renamed over it. Two
    /// writers share the file beside it, so a writer holds the [`RetainLock`] (as
    /// [`Retained::update`] and [`RetainLock::write`] do) unless it is the only one.
    ///
    /// # Errors
    /// The file cannot be written.
    pub fn write(&self, path: &Path) -> Result<(), RetainError> {
        let failed = |source| RetainError::Write {
            path: path.to_owned(),
            source,
        };
        let mut text = serde_json::to_string_pretty(self).map_err(|e| failed(e.into()))?;
        text.push('\n');
        let aside = path.with_extension("json.new");
        let _ = fs::remove_file(&aside);
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&aside)
            .and_then(|mut file| {
                file.write_all(text.as_bytes())?;
                file.sync_all()
            })
            .and_then(|()| fs::rename(&aside, path))
            .map_err(failed)
    }

    /// Read the file at `path`, change it with `change`, and write it back when it changed, all
    /// holding its [`RetainLock`].
    ///
    /// # Errors
    /// The file cannot be locked, read, parsed or written.
    pub fn update(path: &Path, change: impl FnOnce(&mut Self)) -> Result<Self, RetainError> {
        let lock = RetainLock::acquire(path)?;
        let was = lock.read()?;
        let mut now = was.clone();
        change(&mut now);
        if now != was {
            lock.write(&now)?;
        }
        Ok(now)
    }

    /// The releases this file says must stay: the pinned ones and `last_good`.
    #[must_use]
    pub fn protected(&self) -> BTreeSet<CommitId> {
        let mut kept = self.pinned.clone();
        kept.extend(self.last_good.clone());
        kept
    }
}

/// Where `retain.json` is on a host with the installer's layout.
#[must_use]
pub fn path(host: &Layout) -> PathBuf {
    host.join(&host.state_dir()).join(FILE)
}

/// Of the releases `found` (each with the time it was laid out), the ones to remove: every
/// one not in `kept` but the `keep` newest of them. The order is newest first, by the time,
/// then by the commit.
#[must_use]
pub fn surplus(
    found: &[(CommitId, SystemTime)],
    kept: &BTreeSet<CommitId>,
    keep: usize,
) -> Vec<CommitId> {
    let mut old: Vec<(SystemTime, &CommitId)> = found
        .iter()
        .filter(|(commit, _)| !kept.contains(commit))
        .map(|(commit, at)| (*at, commit))
        .collect();
    old.sort_by(|a, b| b.cmp(a));
    old.into_iter().skip(keep).map(|(_, c)| c.clone()).collect()
}

/// A release kept on a host, for `host status` and the local `query`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Kept {
    pub commit: CommitId,
    /// The version text it was installed as, when the host knows it.
    pub version: Option<String>,
    /// Why it must stay, if it must: `current`, `previous`, `on_trial`, `pinned`, `last_good`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub why: Vec<&'static str>,
}

/// What a host knows of the releases it holds, for [`listing`].
#[derive(Clone, Debug, Default)]
pub struct Known<'a> {
    pub current: Option<&'a CommitId>,
    pub previous: Option<&'a CommitId>,
    pub on_trial: Option<&'a CommitId>,
    pub retained: Option<&'a Retained>,
}

/// The releases `found` (each with the time it was laid out), newest first, each with its
/// version and why it must stay.
#[must_use]
pub fn listing(found: &[(CommitId, SystemTime)], known: &Known<'_>) -> Vec<Kept> {
    let mut found: Vec<&(CommitId, SystemTime)> = found.iter().collect();
    found.sort_by(|a, b| (b.1, &b.0).cmp(&(a.1, &a.0)));
    found
        .into_iter()
        .map(|(commit, _)| {
            let is = |c: Option<&CommitId>| c == Some(commit);
            let retained = known.retained;
            let mut why = Vec::new();
            for (word, holds) in [
                ("current", is(known.current)),
                ("previous", is(known.previous)),
                ("on_trial", is(known.on_trial)),
                (
                    "pinned",
                    retained.is_some_and(|r| r.pinned.contains(commit)),
                ),
                (
                    "last_good",
                    retained.is_some_and(|r| r.last_good.as_ref() == Some(commit)),
                ),
            ] {
                if holds {
                    why.push(word);
                }
            }
            Kept {
                commit: commit.clone(),
                version: retained.and_then(|r| r.versions.get(commit).cloned()),
                why,
            }
        })
        .collect()
}

/// One line per kept release, for `host status`: `  <commit> <version|unknown>[ (why, ...)]`.
#[must_use]
pub fn lines(kept: &[Kept]) -> String {
    let mut text = format!("releases kept: {}\n", kept.len());
    for k in kept {
        text.push_str(&format!(
            "  {} {}",
            k.commit,
            k.version.as_deref().unwrap_or("unknown")
        ));
        if !k.why.is_empty() {
            text.push_str(&format!(" ({})", k.why.join(", ")));
        }
        text.push('\n');
    }
    text
}

#[cfg(test)]
#[path = "retain_tests.rs"]
mod tests;
