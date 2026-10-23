//! What the Mac's clean reads from the tools it runs and the decisions it makes from it,
//! as plain functions with no Mac in them, so that they run under test on every
//! platform: how a launchd job is told to be launchd's own, which folders the shared
//! sweep covers, which mounts lie under a slot's folders, and how the kill loop decides
//! the uid has no process left. [`super::macos::host`] runs the tools and calls these.

use std::path::{Path, PathBuf};
use std::time::Duration;

use kbf_mac_session::sweep::SweepPlan;

/// The names a `/var/folders` entry keeps, emptied: `0` (the `DARWIN_USER_DIR`), `C`
/// (cache), `T` (temporary) and `X`, where they exist. Anything else in the entry is
/// removed whole: the slot's user can create files there, and nothing reads them back.
pub(crate) const SKELETON: [&str; 4] = ["0", "C", "T", "X"];

/// The folders whose plists are launchd's own: on the sealed system volume, or Apple's
/// under `/Library` (Rosetta's agents), neither of which a pool user can write.
pub(crate) const OWN_PLISTS: [&str; 2] = ["/System/", "/Library/Apple/"];

/// What `launchctl` exits with for a job or domain that is being torn down: "keep
/// waiting", never "failed".
pub(crate) const TEARING_DOWN: i32 = 124;

/// How many scans the kill loop makes at most.
pub(crate) const MAX_SCANS: usize = 100;

/// The wait between two scans of the kill loop, and between the two looks of the
/// second process look.
pub(crate) const SCAN_PAUSE: Duration = Duration::from_millis(200);

/// The user flags the clean never gives back to an entry root or folder, as bits of
/// `st_flags`: `uchg` and `uappnd` (a lock would stop the next removal) and `hidden`
/// (the system sets it and it hides the entry from the listing the check reads).
/// `nodump` and `opaque` are kept when the system had them.
pub(crate) const NOT_RESTORED: u32 = 0x2 | 0x4 | 0x8000;

/// The flags an entry root or folder is given back, from the `recorded` ones found when
/// the slot was resolved: those without [`NOT_RESTORED`].
pub(crate) fn kept_flags(recorded: u32) -> u32 {
    recorded & !NOT_RESTORED
}

/// Whether the `flags` of a restored entry root hold one the restore never leaves
/// ([`NOT_RESTORED`]). The same set [`kept_flags`] removes, so a root the system gave
/// `nodump` or `opaque` is not found wrong after every clean.
pub(crate) fn unrestored(flags: u32) -> bool {
    flags & NOT_RESTORED != 0
}

/// The places the shared sweep covers: launchd's per-uid files, and every entry the uid
/// owns in the `Shared` folder under `/Users`, `/private/tmp`, `/private/var/tmp` and
/// `/Library/Caches`. Not all of `/private/var/folders`: the uid can write only its own
/// entry there, which the contents step empties.
pub(crate) fn shared_plan() -> SweepPlan {
    let mut plan = SweepPlan::macos();
    plan.owned
        .retain(|place| place != Path::new("/private/var/folders"));
    plan.owned.push(PathBuf::from("/Library/Caches"));
    plan
}

/// Whether the plist at `path` is launchd's own ([`OWN_PLISTS`]).
pub(crate) fn is_own_plist(path: &str) -> bool {
    OWN_PLISTS.iter().any(|folder| path.starts_with(folder))
}

/// The plist path in what `launchctl print user/<uid>/<label>` says: its `path = `
/// line one level in (deeper ones belong to the job's nested dictionaries).
pub(crate) fn plist_path(printed: &str) -> Option<&str> {
    printed
        .lines()
        .find_map(|line| line.strip_prefix("\tpath = "))
        .map(str::trim_end)
}

/// The services `launchctl print user/<uid>` lists: `services = {`, one `<pid, 0 when
/// not running> <last exit or -> <label>` per line, then `}`. A pid of 0 or less is
/// `None`.
pub(crate) fn listed_services(domain: &str) -> Vec<(Option<i32>, String)> {
    domain
        .lines()
        .skip_while(|line| line.trim() != "services = {")
        .skip(1)
        .take_while(|line| line.trim() != "}")
        .filter_map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            let label = (*words.last()?).to_owned();
            let pid = (words.first()?.parse().ok()).filter(|pid: &i32| *pid > 0);
            Some((pid, label))
        })
        .collect()
}

/// The mount points in `mount`'s output (`<device> on <path> (<options>)` per line)
/// that are one of `roots` or lie under one, the deepest first, so that a mount inside
/// a mount comes off before the one it is in.
pub(crate) fn mounts_under(mount: &str, roots: &[&Path]) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = mount
        .lines()
        .filter_map(|line| {
            let (_, rest) = line.split_once(" on ")?;
            let (point, _) = rest.rsplit_once(" (")?;
            Some(PathBuf::from(point))
        })
        .filter(|point| roots.iter().any(|root| point.starts_with(root)))
        .collect();
    found.sort_by_key(|point| std::cmp::Reverse(point.components().count()));
    found
}

/// The kill loop: `kill`, then `scan` the uid's processes, then `pause`, until two scans
/// in a row find none, for at most [`MAX_SCANS`] scans and until `expired` says so. The
/// first scan after a kill can be empty while a process the kill raced with starts one
/// more (a respawn, a fork in flight); the second, [`SCAN_PAUSE`] later, sees it.
///
/// # Errors
/// `kill` or `scan` failed, or processes were left at the bound (their pids).
pub(crate) fn kill_until_quiet(
    expired: &mut dyn FnMut() -> bool,
    kill: &mut dyn FnMut() -> Result<(), String>,
    scan: &mut dyn FnMut() -> Result<Vec<i32>, String>,
    pause: &mut dyn FnMut(),
) -> Result<(), String> {
    let mut quiet = 0;
    let mut left = Vec::new();
    for _ in 0..MAX_SCANS {
        kill()?;
        left = scan()?;
        if left.is_empty() {
            quiet += 1;
            if quiet == 2 {
                return Ok(());
            }
        } else {
            quiet = 0;
        }
        if expired() {
            break;
        }
        pause();
    }
    Err(if left.is_empty() {
        "the uid was not quiet for two scans".to_owned()
    } else {
        format!("processes {left:?} survived SIGKILL")
    })
}

#[cfg(test)]
mod tests;
