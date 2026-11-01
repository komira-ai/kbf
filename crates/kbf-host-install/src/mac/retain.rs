//! The releases a Mac keeps in the kit's `releases/`: the same rule as on Linux
//! ([`kbf_installer::retain`]). Current, previous, every pinned release and the coordinator's
//! `last_good` stay, and so do the newest `keep` others; the rest are removed, only after a swap
//! was confirmed (or, on the ssh path, applied), never while one is on trial. The daemon runs
//! `current` then: the swap restarted it on that release and waited for its welcome, and the
//! release it ran before is `previous`. `retain.json` is in root's private work directory.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::SystemTime;

use kbf_installer::CommitId;
use kbf_installer::retain::{self, Kept, Known, RetainLock, Retained};

use super::kit;
use super::layout::MacHost;
use super::release::is_commit;
use crate::host::take_lock;

/// Where `retain.json` is on a Mac.
pub(crate) fn file(host: &MacHost) -> PathBuf {
    host.work.join(retain::FILE)
}

/// Each directory of `releases/` named by a commit, with the time it was laid out. Anything
/// else there (a `.<commit>.new` a crash left, a stray file or link) is not a release and is left
/// alone.
fn found(host: &MacHost) -> Result<Vec<(CommitId, SystemTime)>, String> {
    let dir = host.releases();
    let items = match fs::read_dir(&dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        listed => listed.map_err(|e| format!("{}: {e}", dir.display()))?,
    };
    let mut found = Vec::new();
    for item in items {
        let item = item.map_err(|e| format!("{}: {e}", dir.display()))?;
        let name = item.file_name();
        let Some(commit) = name
            .to_str()
            .filter(|n| is_commit(n))
            .and_then(|n| CommitId::new(n).ok())
        else {
            continue;
        };
        // An entry gone or unreadable since the listing is not taken for a release: it is left
        // alone, never removed.
        let at = fs::symlink_metadata(item.path())
            .ok()
            .filter(fs::Metadata::is_dir)
            .and_then(|meta| meta.modified().ok());
        if let Some(at) = at {
            found.push((commit, at));
        }
    }
    Ok(found)
}

fn commit(text: Option<&String>) -> Option<CommitId> {
    text.and_then(|t| CommitId::new(t).ok())
}

/// The releases the Mac holds, newest first, each with the version text it was installed as
/// (its `VERSION` file) and why it must stay. `on_trial` is the release a swap put on trial.
pub(crate) fn kept(host: &MacHost, on_trial: Option<&str>) -> Result<Vec<Kept>, String> {
    let links = kit::links(host)?;
    let retained = Retained::read(&file(host)).map_err(|e| e.to_string())?;
    let (current, previous) = (
        commit(links.current.as_ref()),
        commit(links.previous.as_ref()),
    );
    let trial = on_trial.and_then(|c| CommitId::new(c).ok());
    let mut kept = retain::listing(
        &found(host)?,
        &Known {
            current: current.as_ref(),
            previous: previous.as_ref(),
            on_trial: trial.as_ref(),
            retained: Some(&retained),
        },
    );
    for k in &mut kept {
        if let Some(version) = kit::installed_version(host, k.commit.as_str()) {
            k.version = Some(version);
        }
    }
    Ok(kept)
}

/// The release on trial, if the socket's switch left its marker: its commit, or `a release` when
/// the marker cannot be parsed. A marker that cannot be looked at is an error: then whether a
/// release is on trial cannot be told.
fn on_trial(host: &MacHost) -> Result<Option<String>, String> {
    let marker = host.pending_marker();
    let say = |e: io::Error| format!("{}: {e}", marker.display());
    match fs::symlink_metadata(&marker) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(say(e)),
        Ok(_) => {}
    }
    let text = fs::read_to_string(&marker).map_err(say)?;
    let commit = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v["commit"].as_str().map(str::to_owned));
    Ok(Some(commit.unwrap_or_else(|| "a release".to_owned())))
}

/// [`prune`] after an apply of the ssh path, which runs without the host's install lock: it is
/// taken here, without waiting. One held means an install or a switch of the socket is under
/// way, and nothing is removed.
pub(crate) fn prune_after_apply(host: &MacHost, keep: usize) -> Result<String, String> {
    let Ok(_lock) = take_lock(&host.lock_file()) else {
        return Ok("removing nothing: the install lock is held".to_owned());
    };
    prune(host, keep)
}

/// Remove the releases the Mac no longer keeps; what it did, in a line. Nothing is removed while
/// a release is on trial (the socket's marker is there), nor when that cannot be told. The caller
/// holds the host's install lock; `retain.json`'s lock is held from its read to the last removal.
pub(crate) fn prune(host: &MacHost, keep: usize) -> Result<String, String> {
    let lock = RetainLock::acquire(&file(host)).map_err(|e| e.to_string())?;
    if let Some(commit) = on_trial(host)? {
        return Ok(format!("removing nothing: {commit} is on trial"));
    }
    let keep = retain::at_least(keep);
    let links = kit::links(host)?;
    let retained = lock.read().map_err(|e| e.to_string())?;
    let mut must: BTreeSet<CommitId> = retained.protected();
    must.extend(commit(links.current.as_ref()));
    must.extend(commit(links.previous.as_ref()));
    let gone = retain::surplus(&found(host)?, &must, keep);
    for c in &gone {
        let dir = host.release(c.as_str());
        fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let words = |set: Vec<String>| {
        if set.is_empty() {
            "none".to_owned()
        } else {
            set.join(" ")
        }
    };
    Ok(format!(
        "keeping: {} and the {keep} newest others; removing: {}",
        words(must.iter().map(ToString::to_string).collect()),
        words(gone.iter().map(ToString::to_string).collect())
    ))
}
