//! The update itself, over a [`Backend`]: stage a release beside the running one, put it in
//! place on trial, keep it when it is confirmed, go back when it is not.
//!
//! The root of the trust is the marker of what is on trial, which the backend persists in the
//! same step as the move (the marker is written before the first link moves), together with
//! a deadline: [`Ab::tick`] goes back to the release that ran when the deadline passes with no
//! confirm. The deadline is wall-clock time and the marker is on disk, so a restart of this
//! program or a reboot of the host neither loses nor extends it: the first tick after the
//! start finds a deadline that has passed and reverts. Going back needs nothing of the release
//! on trial to run.
//!
//! This program also keeps `state.json` of its own, root's and 0600: the version text each
//! release it staged was staged as (a release does not know it; the commit is all the files
//! hold), the one release staged and not yet switched to, and how the last update ended. It
//! is never the truth about the links; the backend is.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use kbf_installer::retain::{Kept, Retained};

use super::answer::{Answer, Code, LastView, PendingView, QueryView};
use super::backend::{Backend, Fault, Pending};
use super::request::LocalRequest;
use crate::host::take_lock;
use crate::locks;
use crate::mac::release::commit_prefix;
use crate::request::{Channel, Version};
use crate::trust::Trust;

/// How many versions the state file remembers, at the least: it never forgets a release the
/// host keeps (see [`Ab::versions_kept`]).
const KEEP_VERSIONS: usize = 16;

/// The most releases a `query` lists, so its line stays well inside what a client reads (the
/// agent reads 16 KiB).
pub const MAX_LISTED: usize = 64;

/// How an update ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Ended {
    Succeeded,
    Failed,
}

/// Why an update failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Why {
    /// A revert was asked for.
    Reverted,
    /// The deadline passed with no confirm.
    Timeout,
    /// The release could not be put in place.
    SwitchFailed,
}

impl Why {
    fn word(self) -> &'static str {
        match self {
            Self::Reverted => "reverted",
            Self::Timeout => "timeout",
            Self::SwitchFailed => "switch_failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Staged {
    version: String,
    commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Last {
    target: String,
    ended: Ended,
    why: Option<Why>,
}

/// `state.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    /// The version text each commit was staged as.
    #[serde(default)]
    versions: BTreeMap<String, String>,
    /// The release laid out and not yet switched to.
    #[serde(default)]
    staged: Option<Staged>,
    /// How the last update ended.
    #[serde(default)]
    last: Option<Last>,
}

impl Saved {
    /// Remember that `commit` is `version`, forgetting names past `limit` (the commit just
    /// named is never the one forgotten).
    fn note(&mut self, commit: &str, version: &str, limit: usize) {
        self.versions.insert(commit.to_owned(), version.to_owned());
        let surplus = self.versions.len().saturating_sub(limit);
        let forget: Vec<String> = self
            .versions
            .keys()
            .filter(|known| *known != commit)
            .take(surplus)
            .cloned()
            .collect();
        for old in forget {
            self.versions.remove(&old);
        }
    }

    /// The commit `version` was staged as.
    fn commit_of(&self, version: &str) -> Option<&str> {
        self.versions
            .iter()
            .find(|(_, v)| *v == version)
            .map(|(commit, _)| commit.as_str())
    }

    fn version_of(&self, commit: &str) -> Option<String> {
        self.versions.get(commit).cloned()
    }
}

/// The update, over a backend.
pub struct Ab<B: Backend> {
    backend: B,
    state_file: PathBuf,
    trust: Trust,
    /// How long a release on trial has to be confirmed.
    window_secs: u64,
    /// Unix seconds now.
    clock: Box<dyn Fn() -> u64>,
    /// How many releases the host keeps besides the ones that must stay.
    keep: usize,
    /// Where what this program did besides answering is told (a prune after a confirm).
    note: Box<dyn Fn(&str)>,
}

impl<B: Backend> Ab<B> {
    pub fn new(
        backend: B,
        state_file: PathBuf,
        trust: Trust,
        window_secs: u64,
        clock: Box<dyn Fn() -> u64>,
    ) -> Self {
        Self {
            backend,
            state_file,
            trust,
            window_secs,
            clock,
            keep: kbf_installer::retain::DEFAULT_KEEP,
            note: Box::new(|_| {}),
        }
    }

    /// Keep `keep` releases besides the ones that must stay (never fewer than
    /// [`kbf_installer::retain::MIN_KEEP`]), and tell what a prune did to `note`.
    #[must_use]
    pub fn keeping(mut self, keep: usize, note: Box<dyn Fn(&str)>) -> Self {
        self.keep = kbf_installer::retain::at_least(keep);
        self.note = note;
        self
    }

    /// How many versions the state file remembers: every release kept, and more.
    fn versions_kept(&self) -> usize {
        KEEP_VERSIONS.max(self.keep + KEEP_VERSIONS / 2)
    }

    /// Answer a request.
    pub fn handle(&self, request: &LocalRequest) -> Answer {
        let done = match request {
            LocalRequest::Query => self.query(),
            LocalRequest::Install { version, channel } => {
                self.locked(|| self.stage(version, channel))
            }
            LocalRequest::Switch { version } => self.locked(|| self.switch(version)),
            LocalRequest::Confirm { version, last_good } => {
                self.locked(|| self.confirm(version, last_good.as_ref()))
            }
            LocalRequest::Revert => self.locked(|| self.go_back(Why::Reverted)),
        };
        done.unwrap_or_else(Fault::answer)
    }

    /// Go back to the release that ran, if a release is on trial and its deadline has
    /// passed. What it did, for the log; none when it had nothing to do (or the install lock
    /// is held, which the next tick finds again).
    pub fn tick(&self) -> Option<Answer> {
        let pending = self.backend.pending().ok().flatten()?;
        let deadline = pending.deadline_unix?;
        if (self.clock)() < deadline {
            return None;
        }
        let done = self.locked(|| self.go_back(Why::Timeout));
        match done {
            Err(fault) if fault.code == Code::Busy => None,
            other => Some(other.unwrap_or_else(Fault::answer)),
        }
    }

    /// Run `work` holding the install lock, without waiting for it.
    fn locked(&self, work: impl FnOnce() -> Result<Answer, Fault>) -> Result<Answer, Fault> {
        let _lock = take_lock(&self.backend.lock_file()).map_err(|_| {
            Fault::failed(
                Code::Busy,
                "another install, switch or restart holds the install lock",
            )
        })?;
        work()
    }

    fn load(&self) -> Result<Saved, Fault> {
        let unavailable = |why: String| Fault::failed(Code::StateUnavailable, why);
        match fs::symlink_metadata(&self.state_file) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Saved::default()),
            Err(e) => return Err(unavailable(format!("state.json: {e}"))),
            Ok(_) => {}
        }
        let text = self
            .trust
            .read(&self.state_file)
            .map_err(|e| unavailable(format!("state.json: {e}")))?;
        serde_json::from_str(&text).map_err(|e| unavailable(format!("state.json: {e}")))
    }

    fn save(&self, saved: &Saved) -> Result<(), Fault> {
        let unavailable = |why: String| Fault::failed(Code::StateUnavailable, why);
        let text = serde_json::to_string(saved).map_err(|e| unavailable(e.to_string()))?;
        let aside = self.state_file.with_extension("json.new");
        let _ = fs::remove_file(&aside);
        let written = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&aside)
            .and_then(|mut file| {
                file.write_all(text.as_bytes())?;
                file.sync_all()
            })
            .and_then(|()| fs::rename(&aside, &self.state_file));
        written.map_err(|e| unavailable(format!("state.json: {e}")))
    }

    fn query(&self) -> Result<Answer, Fault> {
        let saved = self.load()?;
        let commit = self.backend.current()?;
        let releases = self.listed(&saved);
        let pending = self.backend.pending()?.map(|p| PendingView {
            version: saved.version_of(&p.commit),
            commit: p.commit,
            deadline_unix: p.deadline_unix,
        });
        Ok(Answer::Queried(QueryView {
            version: commit.as_deref().and_then(|c| saved.version_of(c)),
            commit,
            node_id: self.backend.node_id(),
            pending,
            last: saved.last.map(|last| LastView {
                target: last.target,
                outcome: match last.ended {
                    Ended::Succeeded => "succeeded",
                    Ended::Failed => "failed",
                },
                reason: last.why.map(Why::word),
            }),
            busy: locks::flock_held(&self.backend.lock_file()),
            releases,
        }))
    }

    /// The releases the host keeps, each with its version: what the host recorded when it was
    /// laid out, else the text it was staged as here. A listing that cannot be read is empty:
    /// the rest of the answer stands.
    fn listed(&self, saved: &Saved) -> Vec<Kept> {
        let mut kept = self.backend.kept().unwrap_or_default();
        // Past the limit, the releases that must stay are still listed; only the oldest others
        // are left out.
        let must = kept.iter().filter(|k| !k.why.is_empty()).count();
        let mut others = 0;
        kept.retain(|k| {
            others += usize::from(k.why.is_empty());
            !k.why.is_empty() || others + must <= MAX_LISTED
        });
        kept.truncate(MAX_LISTED);
        for k in &mut kept {
            if k.version.is_none() {
                k.version = saved.version_of(k.commit.as_str());
            }
        }
        kept
    }

    fn stage(&self, version: &Version, channel: &Channel) -> Result<Answer, Fault> {
        if self.backend.pending()?.is_some() {
            return Err(Fault::refused(
                Code::Pending,
                "a release is on trial: confirm or revert it first",
            ));
        }
        let current = self.backend.current()?.ok_or_else(|| {
            Fault::refused(Code::NoCurrent, "the host has no release that runs now")
        })?;
        let prefix = commit_prefix(version.as_str()).ok_or_else(|| {
            Fault::refused(
                Code::UnknownRelease,
                "the version carries no commit (+g or +, then 12 hex digits)",
            )
        })?;
        let mut saved = self.load()?;
        let answer = |commit: &str, current: bool| Answer::Staged {
            version: version.as_str().to_owned(),
            commit: commit.to_owned(),
            current,
        };
        if current.starts_with(prefix) {
            // The host runs this release already: all it lacks is the version's text.
            saved.note(&current, version.as_str(), self.versions_kept());
            saved.staged = None;
            self.save(&saved)?;
            return Ok(answer(&current, true));
        }
        let staged = saved
            .staged
            .clone()
            .filter(|s| s.version == version.as_str() && self.backend.has(&s.commit));
        if let Some(staged) = staged {
            return Ok(answer(&staged.commit, false));
        }
        let commit = self.backend.stage(version, channel)?;
        saved.note(&commit, version.as_str(), self.versions_kept());
        saved.staged = Some(Staged {
            version: version.as_str().to_owned(),
            commit: commit.clone(),
        });
        self.save(&saved)?;
        Ok(answer(&commit, false))
    }

    fn switch(&self, version: &Version) -> Result<Answer, Fault> {
        let mut saved = self.load()?;
        let staged = saved
            .staged
            .clone()
            .filter(|s| s.version == version.as_str());
        if let Some(pending) = self.backend.pending()? {
            // The same release again is a repeat; anything else on trial is in the way.
            return match staged {
                Some(s) if s.commit == pending.commit => Ok(Answer::Switched {
                    version: version.as_str().to_owned(),
                    commit: pending.commit,
                    deadline_unix: pending.deadline_unix.unwrap_or(0),
                }),
                _ => Err(Fault::refused(
                    Code::Pending,
                    "another release is on trial: confirm or revert it first",
                )),
            };
        }
        let staged = staged
            .filter(|s| self.backend.has(&s.commit))
            .ok_or_else(|| {
                Fault::refused(Code::NotStaged, "that version is not laid out; install it")
            })?;
        match self
            .backend
            .switch(&staged.commit, self.window_secs, (self.clock)())
        {
            Ok(deadline_unix) => Ok(Answer::Switched {
                version: staged.version,
                commit: staged.commit,
                deadline_unix,
            }),
            Err(fault) => {
                saved.last = Some(Last {
                    target: staged.version,
                    ended: Ended::Failed,
                    why: Some(Why::SwitchFailed),
                });
                // The host is as the fault says; a state file that cannot be written is
                // the lesser news.
                let _ = self.save(&saved);
                Err(fault)
            }
        }
    }

    fn confirm(&self, version: &Version, last_good: Option<&Version>) -> Result<Answer, Fault> {
        let mut saved = self.load()?;
        let confirmed = Answer::Confirmed {
            version: version.as_str().to_owned(),
        };
        let Some(pending) = self.backend.pending()? else {
            // A confirm that was answered and lost is asked again: the release runs, and
            // it was confirmed.
            let kept = self.backend.current()?.is_some_and(|commit| {
                saved.version_of(&commit).as_deref() == Some(version.as_str())
                    && saved.last.as_ref().is_some_and(|last| {
                        last.ended == Ended::Succeeded && last.target == version.as_str()
                    })
            });
            return if kept {
                Ok(confirmed)
            } else {
                Err(Fault::refused(
                    Code::NothingPending,
                    "no release is on trial",
                ))
            };
        };
        if saved.commit_of(version.as_str()) != Some(pending.commit.as_str()) {
            return Err(Fault::refused(
                Code::Pending,
                "another release is on trial than that version",
            ));
        }
        self.backend.confirm(&pending)?;
        saved.last = Some(Last {
            target: version.as_str().to_owned(),
            ended: Ended::Succeeded,
            why: None,
        });
        saved.staged = None;
        self.save(&saved)?;
        self.retain(last_good, &mut saved);
        Ok(confirmed)
    }

    /// After a confirm: record the coordinator's `last_good`, then remove the releases the host
    /// no longer keeps, and forget their versions. The confirm stands whatever this does: a
    /// failure is told to the log, and the next confirm tries again.
    fn retain(&self, last_good: Option<&Version>, saved: &mut Saved) {
        let Some(file) = self.backend.retain_file() else {
            return;
        };
        // The release `last_good` names, if the host holds it; one it does not hold has
        // nothing to keep, and the one named before is no longer the coordinator's. A listing
        // that cannot be read leaves the one recorded as it is.
        let listed = last_good.map(|version| (version, self.backend.kept()));
        match listed {
            Some((version, Ok(kept))) => {
                let held = commit_prefix(version.as_str()).and_then(|prefix| {
                    kept.into_iter()
                        .find_map(|k| k.commit.as_str().starts_with(prefix).then_some(k.commit))
                });
                if let Err(e) = Retained::update(&file, |r| r.last_good = held) {
                    (self.note)(&format!("retain: {e}"));
                }
            }
            Some((_, Err(fault))) => (self.note)(&format!("retain: {}", fault.detail)),
            None => {}
        }
        match self.backend.prune(self.keep) {
            Ok(done) if !done.is_empty() => (self.note)(&format!("prune: {done}")),
            Ok(_) => {}
            Err(fault) => (self.note)(&format!("prune: {}", fault.detail)),
        }
        if let Ok(kept) = self.backend.kept() {
            let before = saved.versions.len();
            saved
                .versions
                .retain(|commit, _| kept.iter().any(|k| k.commit.as_str() == commit));
            if saved.versions.len() != before {
                let _ = self.save(saved);
            }
        }
    }

    /// Put the host back on the release that ran before the one on trial, and remember
    /// that the update failed, and why.
    fn go_back(&self, why: Why) -> Result<Answer, Fault> {
        let pending: Pending = self
            .backend
            .pending()?
            .ok_or_else(|| Fault::refused(Code::NothingPending, "no release is on trial"))?;
        let reverted = self.backend.revert(&pending);
        // A revert that failed with the marker still there moved nothing: the next try finds
        // it. One that failed with the marker gone put the release back and then failed to
        // start it; that update is over all the same.
        if let Err(fault) = &reverted
            && self.backend.pending().ok().flatten().is_some()
        {
            return Err(fault.clone());
        }
        // A state file that cannot be read or written loses the news of the revert, not the
        // revert.
        let mut saved = self.load().unwrap_or_default();
        let target = saved
            .version_of(&pending.commit)
            .unwrap_or_else(|| pending.commit.clone());
        saved.last = Some(Last {
            target,
            ended: Ended::Failed,
            why: Some(why),
        });
        saved.staged = None;
        let _ = self.save(&saved);
        reverted?;
        Ok(Answer::Reverted {
            version: saved.version_of(&pending.from),
        })
    }
}
