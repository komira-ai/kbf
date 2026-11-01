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
use kbf_installer::retain::{self, Kept, Known, Retained};

use super::kit;
use super::layout::MacHost;
use super::release::is_commit;

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

/// Remove the releases the Mac no longer keeps; what it did, in a line. `on_trial` names the
/// release on trial, if one is: then nothing is removed.
pub(crate) fn prune(host: &MacHost, keep: usize, on_trial: Option<&str>) -> Result<String, String> {
    if let Some(commit) = on_trial {
        return Ok(format!("removing nothing: {commit} is on trial"));
    }
    let keep = retain::at_least(keep);
    let links = kit::links(host)?;
    let retained = Retained::read(&file(host)).map_err(|e| e.to_string())?;
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
