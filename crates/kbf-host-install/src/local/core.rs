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
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use kbf_retain::CommitId;
use kbf_retain::{Kept, Retained};

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

/// The most bytes the listing's JSON array takes in a `query` line: with the rest of the line
/// (whose versions are bounded by the request grammar) it stays well inside the 16 KiB the agent
/// reads.
pub const LISTED_BYTES: usize = 10 * 1024;

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

/// At most `max` of the releases `kept` that fit in [`LISTED_BYTES`], the ones that must stay
/// first, and how many are left out ([`kbf_retain::cut`]).
fn cut(kept: Vec<Kept>, max: usize) -> (Vec<Kept>, usize) {
    kbf_retain::cut(kept, max, LISTED_BYTES)
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
            keep: kbf_retain::DEFAULT_KEEP,
            note: Box::new(|_| {}),
        }
    }

    /// Keep `keep` releases besides the ones that must stay (never fewer than
    /// [`kbf_retain::MIN_KEEP`]), and tell what a prune did to `note`.
    #[must_use]
    pub fn keeping(mut self, keep: usize, note: Box<dyn Fn(&str)>) -> Self {
        self.keep = kbf_retain::at_least(keep);
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
        let (releases, releases_omitted, releases_unavailable) = self.listed(&saved);
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
            releases_omitted,
            releases_unavailable,
        }))
    }

    /// The releases the host keeps, each with its version: what the host recorded when it was
    /// laid out, else the text it was staged as here. A listing that cannot be read is empty:
    /// the rest of the answer stands.
    fn listed(&self, saved: &Saved) -> (Vec<Kept>, usize, bool) {
        let Ok(kept) = self.backend.kept() else {
            return (Vec::new(), 0, true);
        };
        let (mut kept, omitted) = cut(kept, MAX_LISTED);
        for k in &mut kept {
            if k.version.is_none() {
                k.version = saved.version_of(k.commit.as_str());
            }
        }
        (kept, omitted, false)
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
        // The last good is recorded while the release is still on trial, when no collection of
        // any program removes a release: one that runs between the confirm and this record
        // would not know the release it names. A last good that cannot be recorded keeps the
        // trial: the confirm fails, every collection still removes nothing, and the confirm
        // asked again (once the file can be written) records it and keeps the release.
        if let Err(why) = self.record(last_good) {
            (self.note)(&format!("retain: {why}; the release stays on trial"));
            return Err(Fault::failed(
                Code::StateUnavailable,
                format!("the last good cannot be recorded: {why}"),
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
        self.retain(&mut saved);
        Ok(confirmed)
    }

    /// Record the last good a confirm names, if it names one, in the host's `retain.json`;
    /// whether it could be (a host that keeps no release directories records nothing, and that
    /// is no failure).
    fn record(&self, last_good: Option<&Version>) -> Result<(), String> {
        match (self.backend.retain_file(), last_good) {
            (Some(file), Some(version)) => self.record_last_good(&file, version),
            _ => Ok(()),
        }
    }

    /// After a confirm: remove the releases the host no longer keeps, and forget their
    /// versions. The confirm stands whatever this does: a failure is told to the log, and the
    /// next confirm tries again.
    fn retain(&self, saved: &mut Saved) {
        if self.backend.retain_file().is_none() {
            return;
        }
        match self.backend.prune(self.keep) {
            Ok(done) if !done.is_empty() => (self.note)(&format!("prune: {done}")),
            Ok(_) => {}
            Err(fault) => (self.note)(&format!("prune: {}", fault.detail)),
        }
        self.seed_versions();
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

    /// Copy into `retain.json` the version text this program's own state knows of each release
    /// the host holds and that file does not, so `host status` names the version `query` does:
    /// a host whose releases were staged before the versions were recorded there. Run when the
    /// socket starts to serve and after each confirm; a failure is told and changes nothing.
    pub fn seed_versions(&self) {
        let Some(file) = self.backend.retain_file() else {
            return;
        };
        let (Ok(saved), Ok(kept)) = (self.load(), self.backend.kept()) else {
            return;
        };
        let known: Vec<(CommitId, String)> = kept
            .into_iter()
            .filter(|k| k.version.is_none())
            .filter_map(|k| saved.version_of(k.commit.as_str()).map(|v| (k.commit, v)))
            .collect();
        if known.is_empty() {
            return;
        }
        let seeded = Retained::update(&file, |r| {
            for (commit, version) in known {
                r.versions.entry(commit).or_insert(version);
            }
        });
        if let Err(e) = seeded {
            (self.note)(&format!("retain: {e}"));
        }
    }

    /// Record `version` as the coordinator's last good release: the one release the host holds
    /// whose commit the version names. One it does not hold has nothing to keep, and the one
    /// recorded before is no longer the coordinator's, so none is recorded.
    ///
    /// # Errors
    /// The listing cannot be read, the version names more than one held release (two commits
    /// with its 12 digits), or the file cannot be written: then what is recorded stays.
    fn record_last_good(&self, file: &Path, version: &Version) -> Result<(), String> {
        let kept = self.backend.kept().map_err(|fault| fault.detail)?;
        let held: Vec<CommitId> = match commit_prefix(version.as_str()) {
            Some(prefix) => kept
                .into_iter()
                .filter(|k| k.commit.as_str().starts_with(prefix))
                .map(|k| k.commit)
                .collect(),
            None => Vec::new(),
        };
        let held = match held.as_slice() {
            [] => None,
            [one] => Some(one.clone()),
            many => {
                let names: Vec<String> = many.iter().map(ToString::to_string).collect();
                return Err(format!(
                    "last_good {} names {} releases: {}",
                    version.as_str(),
                    many.len(),
                    names.join(" ")
                ));
            }
        };
        Retained::update(file, |r| r.last_good = held)
            .map(drop)
            .map_err(|e| e.to_string())
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
