//! The installer's state, `state.json` (schema 2) in the state directory
//! (`/var/lib/kbf-installer` on Linux, `/var/db/kbf-installer` on macOS), the copy of the last
//! good one beside it (`state.json.bak`), and the lock it is changed under.
//!
//! The file is one JSON object: a `schema` number ([`SCHEMA`]) and the fields of [`State`]. It
//! is changed only through a [`Locked`], which holds an exclusive `flock(2)` on the lock file
//! (`/run/kbf-installer/lock`) for as long as it lives, so two runs cannot both read the
//! state, change it and write it back (the second would undo the first). The lock file and the
//! state files are made 0600, root's alone. A write makes a new file beside the old one, syncs
//! it, renames it over the old one and syncs the directory: a reader sees the old state or the
//! new one whole, never part of either, and a power cut after the write returned keeps the
//! new one. That is also why [`status`] and a dry run ([`read_unlocked`]), which only read,
//! need no lock.
//!
//! # The backup rule (S21)
//!
//! Before a write replaces `state.json`, the file it replaces is copied to `state.json.bak`
//! **if it reads back as a state**: the backup is the last good state, never a copy of a
//! damaged file. A read that finds `state.json` missing or damaged uses the backup, and says
//! so ([`Loaded::from_backup`]); nothing is written by the read. If neither file is usable the
//! state is **not** taken to be empty: the read fails with [`StateError::Unusable`] (exit 5,
//! needs an operator) and a write refuses too, so a damaged host is never "repaired" into a
//! fresh one that forgets what it runs. The one case that is empty is a host that was never
//! activated: no state file, no backup, and no `current` or `previous` link.
//!
//! A `state.json` of another schema is [`StateError::Schema`], and the backup is not tried:
//! falling back to an older file would quietly undo a migration.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::CommitId;
use super::layout::{self, Layout};

/// The `schema` number this build writes and the only one it reads.
pub const SCHEMA: u64 = 2;

/// What a host has installed and is doing. Each commit names a directory under `releases/`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    /// The release `current` should point to.
    pub current: Option<CommitId>,
    /// The release `previous` should point to: what a rollback goes back to. Only ever a
    /// confirmed release.
    pub previous: Option<CommitId>,
    /// A release verified and ready to swap in.
    pub staged: Option<CommitId>,
    /// A release swapped in and not yet confirmed.
    pub pending_confirm: Option<PendingConfirm>,
    /// The least an order must be to be taken (see [`Floor`]).
    pub floor: Floor,
    /// The frontend instance that serves, on a frontend host.
    pub serving_instance: Option<String>,
    /// The newest order acknowledged, rebuilt into `status.json` after a reboot.
    pub last_acked_order_id: u64,
    /// The orders taken from `orders/` and not yet acknowledged, so that a restart runs them
    /// again and a stuck record expires by root's own clock.
    pub unacked: Vec<Unacked>,
}

/// An order record the installer has read and not finished with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Unacked {
    pub epoch: u64,
    pub order_id: u64,
    /// When root first saw the record, in seconds since the Unix epoch: never the file's mtime.
    pub first_seen: u64,
    /// The record as read, a copy the installer parses from (never the path again).
    pub record: String,
}

/// The floor under orders: an order is newer than the last acted on when its epoch is higher,
/// or the epoch the same and its id higher (the store counts ids again after a restore), and
/// within an epoch its `generation` is at or above the highest acted on. It replaces the
/// `order_floor` that schema 1 kept and nothing read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Floor {
    pub epoch: u64,
    pub order_id: u64,
    pub generation: u64,
}

/// A swap that rolls back unless it is confirmed ([`crate::activate`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingConfirm {
    /// The activation that made the swap, `<epoch>-<order_id>`; a repeat of it changes
    /// nothing, and a confirm must name it.
    pub activation: String,
    /// The release swapped in.
    pub commit: CommitId,
    /// The release that ran before it, which a rollback restores; `None` on a host's
    /// first deploy.
    pub previous: Option<CommitId>,
    /// Seconds since the Unix epoch by which the confirm must come. `None` until the
    /// window starts: on Linux that is the first start of the new release, which can be
    /// hours after the swap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_wall: Option<u64>,
    /// The same deadline on the monotonic clock, in seconds since boot (a wall clock that
    /// jumps cannot move it). Written by the activation sequence that reads that clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_mono_s: Option<u64>,
    /// How many starts of the new release are still allowed before it is reverted
    /// unconfirmed; `None` where nothing counts starts (macOS).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starts_left: Option<u32>,
}

/// Why the state could not be read, written or locked.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("cannot lock {path}: {source}")]
    Lock { path: PathBuf, source: io::Error },
    /// Another run holds the lock and did not let go within the time given.
    #[error("{path} is held by another run")]
    Busy { path: PathBuf },
    #[error("cannot read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("{path} is not an installer state: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("{path} has schema {found}; this kbf-installer reads only schema {SCHEMA}")]
    Schema { path: PathBuf, found: String },
    #[error("{path} is not an installer state: it has no schema")]
    NoSchema { path: PathBuf },
    #[error("cannot write {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
    /// Neither `state.json` nor `state.json.bak` is usable on a host that was provisioned.
    #[error(
        "the state is unusable and nothing was changed: {primary}; {backup}; \
         an operator must restore {} from a backup",
        layout::STATE_FILE
    )]
    Unusable { primary: String, backup: String },
}

impl StateError {
    /// Whether this needs a person (exit code 5), not a retry.
    pub fn needs_operator(&self) -> bool {
        matches!(self, StateError::Unusable { .. })
    }
}

/// The state read, and where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loaded {
    /// `None`: a host that was never provisioned.
    pub state: Option<State>,
    /// The state is `state.json.bak`'s, because `state.json` was missing or damaged.
    pub from_backup: bool,
}

/// Where the state files are, for a reader that may or may not hold the lock.
#[derive(Clone, Debug)]
struct Files {
    state: PathBuf,
    backup: PathBuf,
    /// What says a host was provisioned: its links.
    links: [PathBuf; 2],
}

impl Files {
    fn of(host: &Layout) -> Files {
        Files {
            state: host.join(&host.state_file()),
            backup: host.join(&host.state_backup()),
            links: [host.join(&host.current()), host.join(&host.previous())],
        }
    }

    /// Whether anything shows the host was activated: a `current` or `previous` link. (Not a
    /// release directory: a release is published before the state that names it is written,
    /// so a first install has one and no state yet.)
    fn provisioned(&self) -> bool {
        self.links
            .iter()
            .any(|l| fs::symlink_metadata(l).is_ok_and(|m| m.file_type().is_symlink()))
    }

    /// The state: the file, else the backup, else (never provisioned) none.
    fn load(&self) -> Result<Loaded, StateError> {
        let (primary, missing) = match read_file(&self.state) {
            Ok(Some(state)) => {
                return Ok(Loaded {
                    state: Some(state),
                    from_backup: false,
                });
            }
            // Another schema is a migration's business, not a damaged file.
            Err(e @ StateError::Schema { .. }) => return Err(e),
            Ok(None) => ("state.json is missing".to_owned(), true),
            Err(e) => (e.to_string(), false),
        };
        match read_file(&self.backup) {
            Ok(Some(state)) => Ok(Loaded {
                state: Some(state),
                from_backup: true,
            }),
            Ok(None) if missing && !self.provisioned() => Ok(Loaded {
                state: None,
                from_backup: false,
            }),
            Ok(None) => Err(StateError::Unusable {
                primary,
                backup: "state.json.bak is missing".to_owned(),
            }),
            Err(e) => Err(StateError::Unusable {
                primary,
                backup: e.to_string(),
            }),
        }
    }
}

/// The deploy lock, held: the only way to read the state for a change and to write it.
/// Dropping it releases the lock.
///
/// An `flock` belongs to the open file description, not to the descriptor, and a
/// description lives until every copy of the descriptor is closed. A process forked while
/// the lock is held (any spawn of a child by another thread of this process) carries a
/// copy until it execs, so closing the file would leave the host locked for as long as
/// that takes. Dropping unlocks first, which no copy outlives, so the lock can be taken
/// again once the drop returns.
#[derive(Debug)]
pub struct Locked {
    lock: File,
    files: Files,
}

impl Drop for Locked {
    fn drop(&mut self) {
        // An error leaves the close that follows to release the lock, as before.
        let _ = self.lock.unlock();
    }
}

impl Locked {
    /// Take the exclusive lock on the layout's lock file (`/run/kbf-installer/lock`; the
    /// directory is made, 0755, when missing), waiting while another run holds it. The lock
    /// file is made (mode 0600) when missing.
    pub fn acquire(host: &Layout) -> Result<Locked, StateError> {
        let path = host.join(&host.lock());
        let fail = |source| StateError::Lock {
            path: path.clone(),
            source,
        };
        use std::os::unix::fs::DirBuilderExt;
        // A lock path always has a parent; the root stands in for the impossible case.
        let dir = path.parent().unwrap_or(std::path::Path::new("/"));
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(dir)
            .map_err(fail)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .map_err(fail)?;
        lock.lock().map_err(fail)?;
        Ok(Locked {
            lock,
            files: Files::of(host),
        })
    }

    /// Like [`Locked::acquire`], but gives up with [`StateError::Busy`] if another run still
    /// holds the lock after `wait`: for the operator's verbs, which report "busy" (exit 4)
    /// rather than queue behind a pass.
    pub fn acquire_within(host: &Layout, wait: Duration) -> Result<Locked, StateError> {
        let path = host.join(&host.lock());
        let fail = |source| StateError::Lock {
            path: path.clone(),
            source,
        };
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .map_err(fail)?;
        let end = Instant::now() + wait;
        while lock.try_lock().is_err() {
            if Instant::now() >= end {
                return Err(StateError::Busy { path });
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(Locked {
            lock,
            files: Files::of(host),
        })
    }

    /// The state, and whether it is the backup's; `None` for a host never provisioned.
    pub fn load(&self) -> Result<Loaded, StateError> {
        self.files.load()
    }

    /// The state; `None` for a host never provisioned (see the module docs for the rest).
    pub fn read(&self) -> Result<Option<State>, StateError> {
        self.files.load().map(|l| l.state)
    }

    /// Replace the state file with `state`, whole (see the module docs): first the last good
    /// `state.json` becomes `state.json.bak`, then the new state replaces `state.json`. The
    /// state directory must exist. Refuses, writing nothing, when the state is unusable.
    pub fn write(&self, state: &State) -> Result<(), StateError> {
        // The same check a read makes: a caller that ignored its error does not get to write.
        let loaded = self.files.load()?;
        let fail = |path: &Path| {
            let path = path.to_owned();
            move |source| StateError::Write { path, source }
        };
        let mut bytes = serde_json::to_vec_pretty(&OnDisk {
            schema: SCHEMA,
            state,
        })
        .expect("a state serializes");
        bytes.push(b'\n');
        // The file read back whole just now (the lock is held), so its bytes are the last good
        // state; when the state came from the backup the file is damaged or gone, and the
        // backup stays as it is.
        if !loaded.from_backup && loaded.state.is_some() {
            let old = fs::read(&self.files.state).map_err(fail(&self.files.state))?;
            replace(&self.files.backup, &old).map_err(fail(&self.files.backup))?;
        }
        replace(&self.files.state, &bytes).map_err(fail(&self.files.state))
    }
}

/// The file as written: the schema number first, then the state's fields.
#[derive(Serialize)]
struct OnDisk<'a> {
    schema: u64,
    #[serde(flatten)]
    state: &'a State,
}

/// Write `bytes` to a new file beside `path` (mode 0600: root's alone), sync it, rename it over `path` and sync
/// the directory.
fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let name = path
        .file_name()
        .expect("a state file has a name")
        .to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    // A copy left by an interrupted run; `create_new` never follows a link there.
    match fs::remove_file(&tmp) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(bytes)?;
    // The two syncs are for a power cut, which no test makes: none would fail without them.
    file.sync_all()?;
    fs::rename(&tmp, path)?;
    let dir = path.parent().expect("the state file is in a directory");
    File::open(dir)?.sync_all()
}

/// The state of the host, read without the lock: for a dry run, which changes nothing
/// and so takes no lock (taking it would make the lock file). `None` when the host was never
/// provisioned.
pub fn read_unlocked(host: &Layout) -> Result<Option<State>, StateError> {
    Files::of(host).load().map(|l| l.state)
}

/// The state in `path`; `None` when there is no such file.
fn read_file(path: &Path) -> Result<Option<State>, StateError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(StateError::Read {
                path: path.to_owned(),
                source,
            });
        }
    };
    parse(path, &bytes).map(Some)
}

/// A state file's bytes: refused unless its `schema` is [`SCHEMA`] and every other
/// field is one of [`State`]'s.
fn parse(path: &Path, bytes: &[u8]) -> Result<State, StateError> {
    let fail = |source| StateError::Parse {
        path: path.to_owned(),
        source,
    };
    let mut object: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(bytes).map_err(fail)?;
    // Valid JSON with no `schema` at all is a damaged file (`{}`), not another schema: the
    // backup is tried. Only an explicit other value is a migration's business.
    let Some(schema) = object.remove("schema") else {
        return Err(StateError::NoSchema {
            path: path.to_owned(),
        });
    };
    if schema != SCHEMA {
        return Err(StateError::Schema {
            path: path.to_owned(),
            found: schema.to_string(),
        });
    }
    serde_json::from_value(serde_json::Value::Object(object)).map_err(fail)
}

/// What the `current` link is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CurrentLink {
    Missing,
    /// A symbolic link, with its target as written.
    To(PathBuf),
    /// Something other than a link: a file or a directory.
    NotALink,
}

/// What `host status` reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// Whether the host has a state (a state file, or its backup); without one, `state` is
    /// the empty state.
    pub exists: bool,
    pub state: State,
    pub link: CurrentLink,
}

impl Status {
    /// Whether `current` points at the state's current release: a link to exactly
    /// `releases/<commit>`, or no link at all when the state has no current release.
    pub fn link_matches(&self) -> bool {
        match (&self.state.current, &self.link) {
            (Some(c), CurrentLink::To(target)) => *target == layout::release_link(c),
            (None, CurrentLink::Missing) => true,
            _ => false,
        }
    }
}

/// The state and the `current` link of the host, read without the lock: a write
/// replaces the state file by a rename, so the file read is one whole state.
pub fn status(host: &Layout) -> Result<Status, StateError> {
    let state = read_unlocked(host)?;
    let link_path = host.join(&host.current());
    let link = current_link(fs::read_link(&link_path)).map_err(|source| StateError::Read {
        path: link_path,
        source,
    })?;
    Ok(Status {
        exists: state.is_some(),
        state: state.unwrap_or_default(),
        link,
    })
}

/// What `read_link` of `current` says: EINVAL is something there that is not a link.
fn current_link(read: io::Result<PathBuf>) -> io::Result<CurrentLink> {
    match read {
        Ok(target) => Ok(CurrentLink::To(target)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(CurrentLink::Missing),
        Err(e) if e.kind() == io::ErrorKind::InvalidInput => Ok(CurrentLink::NotALink),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
pub(crate) mod tests;
