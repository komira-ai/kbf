//! `activate`: make a release the host keeps the one that runs, for good, by this program
//! alone. The host-local way back when a release misbehaves and the coordinator is down or must
//! not be waited for (`docs/deploy/host-install.md`, "The emergency command").
//!
//! - **No fetch.** The release must be one the host keeps ([`Backend::kept`]); one it does not
//!   keep, or a name that fits two, is refused with the releases it keeps
//!   ([`Answer::Unresolved`]), in the listing's format.
//! - **No trial.** There is no deadline and no revert: `previous` becomes the release left, and a
//!   release that was on trial is on trial no more.
//! - **Nothing of the release left runs.** This program moves the links and the service manager
//!   restarts the units (the daemon's launchd job on a Mac) on the release named; the release
//!   being left is never executed by the swap.
//! - **The install lock.** It is taken without waiting, as every install, switch and restart
//!   takes it. A holder is refused and named ([`locks::Holder`]). `--force-lock` takes it from a
//!   holder that is gone or stuck (the lock file is made anew, [`crate::host::take_lock_over`]),
//!   and the take is appended to [`LOCK_TAKES`] beside the state file at once, before anything
//!   else is read or changes. The locks taken inside it (`retain.json`'s, the installer's
//!   state) are taken without waiting too: a holder that does not let go is refused, not waited
//!   for.
//! - **A release that cannot run is not kept.** The release named must be one the backend can
//!   start ([`Backend::has`]: its binary is there, and on a Mac root's); one that is not is
//!   refused with the listing, before anything changes.
//! - **The pin** (`--pin`) writes the hold file for the agent ([`Hold`], root's, 0644) and pins
//!   the release in `retain.json`, so no prune removes it, once the links name the release (a
//!   refused or failed swap pins nothing; a swap whose units did not come up is pinned). A hold
//!   file whose directory is not there, or a `retain.json` another writer holds, is refused
//!   before anything changes. The hold file is written first, then `retain.json`: a pin that
//!   fails between them leaves the hold and is told; asked again, both are written. The agent's
//!   reading of the hold file is the agent's.
//! - **The unpin** ([`Ab::unpin`], `kbf-host-install unpin`) undoes a pin under the same
//!   install lock, taken without waiting: the commit root's hold file names is taken out of
//!   `retain.json`'s `pinned` first, then the hold file is removed, so a run cut short between
//!   the two leaves the hold file and is finished by asking again. A host with no hold file is
//!   not pinned, and the unpin says so and changes nothing. A hold file that is not root's, or
//!   names no commit, is removed and unpins nothing in `retain.json`.
//!
//! The work is one code path over `&dyn Backend` ([`Activation`]), whatever the backend.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

use kbf_retain::{CommitId, Kept, RetainLock, Retained};
use rustix::fs::{AtFlags, Mode, OFlags};
use serde::Serialize;

use super::{Ab, Ended, Last, MAX_LISTED, Saved, cut, load_saved, save_saved};
use crate::host::{Lock, take_lock, take_lock_over};
use crate::local::answer::{Answer, Code};
use crate::local::backend::{Backend, Fault};
use crate::local::request::Target;
use crate::locks::{self, Holder};
use crate::outcome::{FailureCode, Outcome};
use crate::trust::{Trust, nofollow};

/// The file, beside the state file, that records each take of the install lock from another
/// holder: one JSON line per take ([`Take`]), appended, root's and 0600. It is a file of its own
/// because `state.json` is read strictly by the releases of this program before this one.
pub const LOCK_TAKES: &str = "lock-takes.jsonl";

/// How an `activate` was asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct How {
    /// Write the hold file and pin the release (`--pin`).
    pub pin: bool,
    /// Take the install lock from another holder (`--force-lock`).
    pub force_lock: bool,
}

/// One line of [`LOCK_TAKES`].
#[derive(Debug, Serialize)]
struct Take<'a> {
    /// When, in Unix seconds.
    at_unix: u64,
    /// The verb that took it.
    by: &'static str,
    /// The release the verb was asked for, as it was given.
    target: &'a str,
    /// The process id the holder had written into the lock file, if one.
    holder_pid: Option<u32>,
    /// Whether a process with that id was alive.
    holder_alive: bool,
}

/// The hold file an `activate --pin` writes, for the agent:
/// `{"commit":C,"version":V|null,"at_unix":N,"by":"activate"}`. While it names a commit the
/// agent is to leave the host on it.
#[derive(Debug, Serialize)]
struct Hold<'a> {
    commit: &'a str,
    version: Option<&'a str>,
    at_unix: u64,
    by: &'static str,
}

impl<B: Backend> Ab<B> {
    /// Make the kept release `target` names the one that runs, for good (see the module docs).
    pub fn activate(&self, target: &Target, how: How) -> Answer {
        Activation {
            backend: &self.backend,
            state_file: &self.state_file,
            trust: &self.trust,
            clock: &*self.clock,
            note: &*self.note,
            hold_file: self.hold_file.as_deref(),
            versions_kept: self.versions_kept(),
        }
        .run(target, how)
    }

    /// Undo the pin of an `activate --pin` (see the module docs).
    pub fn unpin(&self) -> Answer {
        Activation {
            backend: &self.backend,
            state_file: &self.state_file,
            trust: &self.trust,
            clock: &*self.clock,
            note: &*self.note,
            hold_file: self.hold_file.as_deref(),
            versions_kept: self.versions_kept(),
        }
        .unpin()
    }
}

/// What an activate needs of the core, borrowed: one code path for every backend.
struct Activation<'a> {
    backend: &'a dyn Backend,
    state_file: &'a Path,
    trust: &'a Trust,
    /// Unix seconds now.
    clock: &'a dyn Fn() -> u64,
    note: &'a dyn Fn(&str),
    hold_file: Option<&'a Path>,
    versions_kept: usize,
}

impl Activation<'_> {
    fn run(&self, target: &Target, how: How) -> Answer {
        let hold = match (how.pin, self.hold_file) {
            (true, None) => {
                return Answer::Refused(
                    Code::ConfigInvalid,
                    "this host has no hold file".to_owned(),
                );
            }
            (true, Some(hold)) => Some(hold),
            (false, _) => None,
        };
        let (lock, holder) = match self.lock(how.force_lock, true) {
            Ok(taken) => taken,
            Err(answer) => return *answer,
        };
        let done = self.held(target, hold, holder);
        drop(lock);
        done.unwrap_or_else(|answer| *answer)
    }

    /// The install lock, taken without waiting, or from its holder when `force`; the holder it
    /// was taken from. A holder that keeps it is named in the refusal, which offers
    /// `--force-lock` when the verb takes it (`forcible`). (With `force`, a lock still busy is
    /// one another taker made anew at the same moment.)
    fn lock(&self, force: bool, forcible: bool) -> Result<(Lock, Option<Holder>), Box<Answer>> {
        let path = self.backend.lock_file();
        let taken = if force {
            take_lock_over(&path)
        } else {
            take_lock(&path).map(|lock| (lock, None))
        };
        taken.map_err(|outcome| {
            Box::new(match outcome {
                Outcome::Failed(failure) if failure.code == FailureCode::Busy => {
                    let holder = locks::holder(&path);
                    let what_now = match (holder.alive, forcible) {
                        (true, true) => {
                            "let it end or stop it, or take it from a stuck holder with --force-lock"
                        }
                        (false, true) => "take it with --force-lock",
                        (true, false) => "let it end or stop it",
                        (false, false) => "take it with activate --force-lock",
                    };
                    Answer::Refused(
                        Code::Busy,
                        format!(
                            "the install lock {} is held by {}: {what_now}",
                            path.display(),
                            holder.describe()
                        ),
                    )
                }
                other => Answer::Failed(
                    Code::StateUnavailable,
                    other.stderr_detail().unwrap_or_default(),
                ),
            })
        })
    }

    /// The work of an activate, holding the install lock.
    fn held(
        &self,
        target: &Target,
        hold: Option<&Path>,
        holder: Option<Holder>,
    ) -> Result<Answer, Box<Answer>> {
        let refuse = |fault: Fault| Box::new(fault.answer());
        // A take of the lock is recorded first: the lock file is already made anew, whatever
        // the activate does next.
        if let Some(holder) = &holder {
            self.record_take(target, holder).map_err(refuse)?;
        }
        if let Some(hold) = hold {
            self.can_pin(hold).map_err(refuse)?;
        }
        let mut saved = load_saved(self.state_file, self.trust).map_err(refuse)?;
        let (chosen, kept) = self.resolve(target, &saved)?;
        let commit = chosen.commit.as_str();
        if !self.backend.has(commit) {
            return Err(unresolved(
                Code::NotKept,
                format!(
                    "{:?} names {commit}, whose binary this host cannot run",
                    target.as_str()
                ),
                kept,
            ));
        }
        let from = self.backend.current().map_err(refuse)?;
        let swapped = self.backend.activate(commit);
        if let Err(fault) = &swapped
            && fault.code != Code::RestartFailed
        {
            return Err(refuse(fault.clone()));
        }
        // The links name the release now (a `restart_failed` too): the state and the pin follow.
        self.record(&mut saved, &chosen, swapped.is_ok());
        let pinned = hold.map(|hold| self.pin(hold, &chosen));
        if let Some(Err(fault)) = &pinned {
            (self.note)(&format!("activate: the pin: {}", fault.detail));
        }
        swapped.map_err(refuse)?;
        if let Some(Err(fault)) = pinned {
            return Err(Box::new(Answer::Failed(
                Code::StateUnavailable,
                format!("{commit} runs now, but it is not pinned: {}", fault.detail),
            )));
        }
        Ok(Answer::Activated {
            commit: chosen.commit.clone(),
            version: chosen.version.clone(),
            from: from.and_then(|c| CommitId::new(&c).ok()),
            pinned: hold.is_some(),
            lock_taken: holder.is_some(),
        })
    }

    /// The one kept release `target` names, and the listing; a refusal with the listing when it
    /// names none or more than one.
    fn resolve(&self, target: &Target, saved: &Saved) -> Result<(Kept, Vec<Kept>), Box<Answer>> {
        let mut kept = self
            .backend
            .kept()
            .map_err(|fault| Box::new(fault.answer()))?;
        for k in &mut kept {
            if k.version.is_none() && !k.version_left_out {
                k.version = saved.version_of(k.commit.as_str());
            }
        }
        let named: Vec<&Kept> = kept
            .iter()
            .filter(|k| target.names(k.commit.as_str(), k.version.as_deref()))
            .collect();
        match named.as_slice() {
            [one] => {
                let one = (*one).clone();
                Ok((one, kept))
            }
            [] => Err(unresolved(
                Code::NotKept,
                format!("{:?} names no release this host keeps", target.as_str()),
                kept,
            )),
            many => {
                let names: Vec<&str> = many.iter().map(|k| k.commit.as_str()).collect();
                Err(unresolved(
                    Code::AmbiguousRelease,
                    format!(
                        "{:?} names more than one release this host keeps: {}",
                        target.as_str(),
                        names.join(" ")
                    ),
                    kept,
                ))
            }
        }
    }

    /// Record in `state.json` the release the links name now: its version, no `staged` that
    /// names it, and, when its units came up, `last`. (`last` says how an update ended in words
    /// older helpers read; a release whose units did not come up has none of its own, and keeps
    /// the last one.) The release runs: a state file that cannot be written is the lesser news.
    fn record(&self, saved: &mut Saved, chosen: &Kept, came_up: bool) {
        let commit = chosen.commit.as_str();
        if let Some(version) = &chosen.version {
            saved.note(commit, version, self.versions_kept);
        }
        if saved.staged.as_ref().is_some_and(|s| s.commit == commit) {
            saved.staged = None;
        }
        if came_up {
            saved.last = Some(Last {
                target: chosen.version.clone().unwrap_or_else(|| commit.to_owned()),
                ended: Ended::Succeeded,
                why: None,
            });
        }
        if let Err(fault) = save_saved(self.state_file, saved) {
            (self.note)(&format!("activate: {}", fault.detail));
        }
    }

    /// Append the take of the install lock from `holder` to [`LOCK_TAKES`]. A record left cut
    /// short (a crash in the middle of a line) is ended first, so each record is a line of its
    /// own.
    fn record_take(&self, target: &Target, holder: &Holder) -> Result<(), Fault> {
        let path = self.state_file.with_file_name(LOCK_TAKES);
        // Serializing numbers, booleans and strings does not fail.
        let line = serde_json::to_string(&Take {
            at_unix: (self.clock)(),
            by: "activate",
            target: target.as_str(),
            holder_pid: holder.pid,
            holder_alive: holder.alive,
        })
        .unwrap_or_default();
        let appended = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(nofollow())
            .open(&path)
            .and_then(|mut file| {
                let len = file.metadata()?.len();
                let mut last = [b'\n'];
                if len > 0 {
                    file.read_exact_at(&mut last, len - 1)?;
                }
                let start = if last[0] == b'\n' { "" } else { "\n" };
                file.write_all(format!("{start}{line}\n").as_bytes())?;
                file.sync_all()
            });
        appended.map_err(|e| {
            Fault::failed(
                Code::StateUnavailable,
                format!("the take of the install lock cannot be recorded: {LOCK_TAKES}: {e}"),
            )
        })
    }

    /// Whether a pin can be written once the links have moved: the hold file's directory is there,
    /// and no other writer holds `retain.json`. A refusal when not; nothing changes either way.
    fn can_pin(&self, hold: &Path) -> Result<(), Fault> {
        let dir = hold.parent().unwrap_or(Path::new(""));
        if !dir.is_dir() {
            return Err(Fault::refused(
                Code::ConfigInvalid,
                format!(
                    "the hold file's directory {:?} is not there: without an agent, leave out --pin",
                    dir.display().to_string()
                ),
            ));
        }
        match self
            .backend
            .retain_file()
            .map(|f| RetainLock::try_acquire(&f))
        {
            Some(Err(e)) => Err(Fault::refused(Code::Busy, e.to_string())),
            _ => Ok(()),
        }
    }

    /// Write the hold file `hold` naming `kept`, then pin it in `retain.json`, without waiting
    /// for its lock.
    fn pin(&self, hold: &Path, kept: &Kept) -> Result<(), Fault> {
        let unavailable = |what: &str, e: &dyn std::fmt::Display| {
            Fault::failed(Code::StateUnavailable, format!("{what}: {e}"))
        };
        // Serializing numbers and strings does not fail.
        let text = serde_json::to_string(&Hold {
            commit: kept.commit.as_str(),
            version: kept.version.as_deref(),
            at_unix: (self.clock)(),
            by: "activate",
        })
        .unwrap_or_default();
        write_hold(hold, &text, &|_| {})
            .map_err(|e| unavailable(&hold.display().to_string(), &e))?;
        if let Some(file) = self.backend.retain_file() {
            Retained::try_update(&file, |r| {
                r.pinned.insert(kept.commit.clone());
            })
            .map_err(|e| unavailable("the pin", &e))?;
        }
        Ok(())
    }
}

impl Activation<'_> {
    /// Undo the pin, holding the install lock (see the module docs).
    fn unpin(&self) -> Answer {
        let Some(hold) = self.hold_file else {
            return Answer::Refused(Code::ConfigInvalid, "this host has no hold file".to_owned());
        };
        let lock = match self.lock(false, false) {
            Ok((lock, _)) => lock,
            Err(answer) => return *answer,
        };
        let done = self.unpin_held(hold);
        drop(lock);
        done.unwrap_or_else(Fault::answer)
    }

    fn unpin_held(&self, hold: &Path) -> Result<Answer, Fault> {
        let commit = match read_hold(hold, self.trust) {
            Read::Absent => return Ok(Answer::Unpinned { commit: None }),
            Read::Commit(commit) => Some(commit),
            Read::NoCommit(why) => {
                (self.note)(&format!("unpin: {why}: nothing is unpinned in retain.json"));
                None
            }
        };
        if let (Some(commit), Some(file)) = (&commit, self.backend.retain_file()) {
            Retained::try_update(&file, |r| {
                r.pinned.remove(commit);
            })
            .map_err(|e| match e {
                kbf_retain::RetainError::Busy { .. } => Fault::refused(Code::Busy, e.to_string()),
                other => Fault::failed(Code::StateUnavailable, format!("the pin: {other}")),
            })?;
        }
        remove_hold(hold).map_err(|e| {
            Fault::failed(Code::StateUnavailable, format!("{}: {e}", hold.display()))
        })?;
        Ok(Answer::Unpinned { commit })
    }
}

/// What a hold file holds, for an unpin.
enum Read {
    /// There is none.
    Absent,
    /// Root's, naming this commit.
    Commit(CommitId),
    /// Something is at the name, but it is not root's or names no commit, for this reason.
    NoCommit(String),
}

/// The keys of a hold file an unpin reads.
#[derive(serde::Deserialize)]
struct HoldRead {
    commit: String,
}

/// Read the hold file `path`, never through a link, trusting only a regular file `trust` owns.
fn read_hold(path: &Path, trust: &Trust) -> Read {
    let opened = OpenOptions::new()
        .read(true)
        .custom_flags(nofollow() | libc::O_NONBLOCK)
        .open(path);
    let file = match opened {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Read::Absent,
        Err(e) => return Read::NoCommit(format!("{}: {e}", path.display())),
    };
    let text = file
        .metadata()
        .and_then(|meta| {
            if meta.is_file() && trust.owns(&meta) {
                let mut text = String::new();
                io::Read::read_to_string(&mut io::Read::take(file, 4096), &mut text)?;
                Ok(text)
            } else {
                Err(io::Error::other("not a regular file root owns"))
            }
        })
        .map_err(|e| format!("{}: {e}", path.display()));
    match text.and_then(|text| {
        serde_json::from_str::<HoldRead>(&text)
            .map_err(|e| e.to_string())
            .and_then(|hold| CommitId::new(&hold.commit).map_err(|e| e.to_string()))
    }) {
        Ok(commit) => Read::Commit(commit),
        Err(why) => Read::NoCommit(why),
    }
}

/// Remove the name `path` (never following a link) and sync its directory; no file is fine.
fn remove_hold(path: &Path) -> io::Result<()> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io::Error::other(
            "the hold file has no directory or no name",
        ));
    };
    let dir = rustix::fs::open(
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    match rustix::fs::unlinkat(&dir, name, AtFlags::empty()) {
        Ok(()) | Err(rustix::io::Errno::NOENT) => {}
        Err(e) => return Err(e.into()),
    }
    rustix::fs::fsync(&dir)?;
    Ok(())
}

/// A refusal of `code` that tells `detail` and lists the releases the host keeps.
fn unresolved(code: Code, detail: String, kept: Vec<Kept>) -> Box<Answer> {
    let (releases, releases_omitted) = cut(kept, MAX_LISTED);
    Box::new(Answer::Unresolved {
        code,
        detail,
        releases,
        releases_omitted,
    })
}

/// Write `text` as the file `path`, root's and 0644, in a directory that root does not own (the
/// agent's): through the directory's descriptor, opened without following a link, the file made
/// new aside (never following a link there either), renamed over the name, and the directory
/// synced. Whatever the directory's owner put in the way is replaced, never written through; one
/// that swaps the file aside before the rename is found (the name then holds another file than
/// the one written) and is an error. `before_rename` runs between the write and the rename, with
/// the directory's descriptor (a test's way in; nothing in production).
fn write_hold(
    path: &Path,
    text: &str,
    before_rename: &dyn Fn(&rustix::fd::OwnedFd),
) -> io::Result<()> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io::Error::other(
            "the hold file has no directory or no name",
        ));
    };
    let aside = format!(".{}.new", name.to_string_lossy());
    let dir = rustix::fs::open(
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    match rustix::fs::unlinkat(&dir, aside.as_str(), AtFlags::empty()) {
        Ok(()) | Err(rustix::io::Errno::NOENT) => {}
        Err(e) => return Err(e.into()),
    }
    let mode = Mode::from_raw_mode(0o644);
    let fd = rustix::fs::openat(
        &dir,
        aside.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        mode,
    )?;
    // The mode is exactly 0644, whatever the umask.
    rustix::fs::fchmod(&fd, mode)?;
    let mut file = File::from(fd);
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    before_rename(&dir);
    rustix::fs::renameat(&dir, aside.as_str(), &dir, name)?;
    let written = file.metadata()?;
    let named = rustix::fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
    if (named.st_dev, named.st_ino) != (written.dev() as _, written.ino() as _) {
        return Err(io::Error::other(
            "the hold file was replaced while it was written",
        ));
    }
    rustix::fs::fsync(&dir)?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn write_hold_for_test(
    path: &Path,
    text: &str,
    before_rename: &dyn Fn(&rustix::fd::OwnedFd),
) -> io::Result<()> {
    write_hold(path, text, before_rename)
}
