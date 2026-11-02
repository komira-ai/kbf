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
//!   holder that is gone or stuck (the lock file is made anew), and the take is appended to
//!   [`LOCK_TAKES`] beside the state file at once, before anything else is read or changes.
//! - **The pin** (`--pin`) writes the hold file for the agent ([`Hold`], root's, 0644) and pins
//!   the release in `retain.json`, so no prune removes it, once the links name the release (a
//!   refused or failed swap pins nothing; a swap whose units did not come up is pinned). A hold
//!   file whose directory is not there is refused before anything changes. The agent's reading
//!   of the hold file is the agent's.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use kbf_retain::{Kept, Retained};
use rustix::fs::{AtFlags, Mode, OFlags};
use serde::Serialize;

use super::{Ab, Ended, Last, MAX_LISTED, cut};
use crate::host::{Lock, take_lock, take_lock_over};
use crate::local::answer::{Answer, Code};
use crate::local::backend::{Backend, Fault};
use crate::local::request::Target;
use crate::locks::{self, Holder};
use crate::outcome::{FailureCode, Outcome};
use crate::trust::nofollow;

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
        let hold = match (how.pin, &self.hold_file) {
            (true, None) => {
                return Answer::Refused(
                    Code::ConfigInvalid,
                    "this host has no hold file".to_owned(),
                );
            }
            (true, Some(hold)) => Some(hold.as_path()),
            (false, _) => None,
        };
        let (lock, holder) = match self.activate_lock(how.force_lock) {
            Ok(taken) => taken,
            Err(answer) => return *answer,
        };
        let done = self.activate_held(target, hold, holder);
        drop(lock);
        done.unwrap_or_else(|answer| *answer)
    }

    /// The install lock, taken without waiting, or from its holder when `force`; the holder it
    /// was taken from. A holder that keeps it is named in the refusal.
    fn activate_lock(&self, force: bool) -> Result<(Lock, Option<Holder>), Box<Answer>> {
        // With `force`, a lock still busy is one another taker made anew at the same moment.
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
                    let what_now = if holder.alive {
                        "let it end or stop it, or take it from a stuck holder with --force-lock"
                    } else {
                        "take it with --force-lock"
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

    /// The work of [`Ab::activate`], holding the install lock.
    fn activate_held(
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
        if let Some(dir) = hold.map(|hold| hold.parent().unwrap_or(Path::new("")))
            && !dir.is_dir()
        {
            return Err(Box::new(Answer::Refused(
                Code::ConfigInvalid,
                format!(
                    "the hold file's directory {:?} is not there: without an agent, leave out \
                         --pin",
                    dir.display().to_string()
                ),
            )));
        }
        let mut saved = self.load().map_err(refuse)?;
        let mut kept = self.backend.kept().map_err(refuse)?;
        for k in &mut kept {
            if k.version.is_none() && !k.version_left_out {
                k.version = saved.version_of(k.commit.as_str());
            }
        }
        let named: Vec<&Kept> = kept
            .iter()
            .filter(|k| target.names(k.commit.as_str(), k.version.as_deref()))
            .collect();
        let chosen = match named.as_slice() {
            [one] => (*one).clone(),
            many => {
                let code = if many.is_empty() {
                    Code::NotKept
                } else {
                    Code::AmbiguousRelease
                };
                let (releases, releases_omitted) = cut(kept, MAX_LISTED);
                return Err(Box::new(Answer::Unresolved {
                    code,
                    target: target.as_str().to_owned(),
                    releases,
                    releases_omitted,
                }));
            }
        };
        let commit = chosen.commit.as_str();
        let from = self.backend.current().map_err(refuse)?;
        let swapped = self.backend.activate(commit);
        if let Err(fault) = &swapped
            && fault.code != Code::RestartFailed
        {
            return Err(refuse(fault.clone()));
        }
        // The links name the release now (a `restart_failed` too): the state and the pin follow.
        if let Some(version) = &chosen.version {
            saved.note(commit, version, self.versions_kept());
        }
        if saved.staged.as_ref().is_some_and(|s| s.commit == commit) {
            saved.staged = None;
        }
        // `last` says how an update ended in words older helpers read; a release whose units did
        // not come up has none of its own, and keeps the last one.
        if swapped.is_ok() {
            saved.last = Some(Last {
                target: chosen.version.clone().unwrap_or_else(|| commit.to_owned()),
                ended: Ended::Succeeded,
                why: None,
            });
        }
        // The release runs: a state file that cannot be written is the lesser news.
        if let Err(fault) = self.save(&saved) {
            (self.note)(&format!("activate: {}", fault.detail));
        }
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
            commit: commit.to_owned(),
            version: chosen.version.clone(),
            from,
            pinned: hold.is_some(),
            lock_taken: holder.is_some(),
        })
    }

    /// Append the take of the install lock from `holder` to [`LOCK_TAKES`].
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
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(nofollow())
            .open(&path)
            .and_then(|mut file| {
                writeln!(file, "{line}")?;
                file.sync_all()
            });
        appended.map_err(|e| {
            Fault::failed(
                Code::StateUnavailable,
                format!("the take of the install lock cannot be recorded: {LOCK_TAKES}: {e}"),
            )
        })
    }

    /// Write the hold file `hold` naming `kept`, then pin it in `retain.json`.
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
        write_hold(hold, &text).map_err(|e| unavailable(&hold.display().to_string(), &e))?;
        if let Some(file) = self.backend.retain_file() {
            Retained::update(&file, |r| {
                r.pinned.insert(kept.commit.clone());
            })
            .map_err(|e| unavailable("the pin", &e))?;
        }
        Ok(())
    }
}

/// Write `text` as the file `path`, root's and 0644, in a directory that root does not own (the
/// agent's): through the directory's descriptor, opened without following a link, the file made
/// new aside (never following a link there either) and renamed over the name. Whatever the
/// directory's owner put in the way is replaced, never written through.
fn write_hold(path: &Path, text: &str) -> io::Result<()> {
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
    rustix::fs::renameat(&dir, aside.as_str(), &dir, name)?;
    Ok(())
}
