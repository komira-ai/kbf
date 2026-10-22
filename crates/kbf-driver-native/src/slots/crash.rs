//! C9 of the clean: the crash reports about the slot's uid. A process of a pool user that
//! crashes leaves a report outside the slot's home: in the system's crash folder, in the
//! console user's own, or a core file in `/cores`. The next lease would read it, and a
//! report names the crashed program's arguments and paths.
//!
//! **Attribution.** A report is the uid's when the file is **owned by the uid**, or when it
//! is a regular file that is an `.ips` report (it starts with `{`: two JSON values, a
//! header and a body) and has the uid as the `userID` key of one of those two objects.
//! JSON escapes every string, so nothing a process puts in its path or name can become a
//! key of those objects. A **text** report is attributed by owner only: its `User ID:`
//! line sits in a block of lines that a process path containing a newline can forge, so
//! the number in it names no one. How `ReportCrash` attributes a report to a user when
//! the crashed process is not the console user's, and where it writes one on a Mac with
//! no console user, is not measured (experiments X6 and G18 of the Mac pool plan): a
//! report this rule cannot attribute stays.
//!
//! **Where root walks.** The folders are other users' to write (the console user's own
//! `DiagnosticReports`), and the clean removes as root, so no path is ever followed:
//! every folder is opened component by component from a root descriptor with
//! `O_NOFOLLOW`, each entry is judged with `fstatat` on the folder's descriptor, read with
//! `openat(O_NOFOLLOW)` on it, and removed with `unlinkat` on it. A link in the path or
//! in the folder is not followed, whenever it was put there. A folder with a **pool-owned
//! directory on its path** is skipped (a pool user controls it), and so is one that
//! cannot be opened. An error inside a folder that is neither root's nor the uid's (a
//! folder another user owns) is logged and skipped, not a failure of this slot's clean.
//! What is read of reports in all is capped ([`READ_BUDGET`]).

use std::ffi::CString;
use std::io::Read as _;
use std::ops::RangeInclusive;
use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags, Stat};

/// The crash folders of the Mac itself, and the core file folder.
const SYSTEM: [&str; 4] = [
    "/Library/Logs/DiagnosticReports",
    "/Library/Logs/DiagnosticReports/Retired",
    "/cores",
    "/private/var/root/Library/Logs/DiagnosticReports",
];

const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

const FILE: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);

/// Where a report about a pool user's process can be: [`SYSTEM`], and the
/// `DiagnosticReports` folder, and its `Retired` one, of every user named in `users`
/// (the names in `/Users`; the console user's crash reporter writes there, whoever
/// crashed). A name is only a name: the walk skips one that is a link or a pool user's.
pub(crate) fn crash_dirs(users: &[String]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = SYSTEM.iter().map(PathBuf::from).collect();
    for user in users {
        let reports = Path::new("/Users")
            .join(user)
            .join("Library/Logs/DiagnosticReports");
        dirs.push(reports.join("Retired"));
        dirs.push(reports);
    }
    dirs
}

/// How much of a report is read at first: enough to see whether it is JSON.
pub(crate) const HEAD: usize = 16 * 1024;

/// How much of an `.ips` report is read: both of its JSON values must be whole to be
/// parsed, and a report larger than this names no one.
pub(crate) const WHOLE: usize = 4 * 1024 * 1024;

/// The most one sweep or one check reads of reports, in all: a folder of large files that
/// start with `{` cannot stall the clean. A report past it is not read, so not attributed.
pub(crate) const READ_BUDGET: usize = 32 * 1024 * 1024;

/// Whose reports, and whose folders are not to be walked.
pub(crate) struct Scope {
    /// The slot's uid.
    pub uid: u32,
    /// The uids of the pool's users: a directory one owns on a folder's path makes it
    /// theirs to rewrite, and it is skipped.
    pub pool: RangeInclusive<u32>,
}

/// Whether the crash report `text` is an `.ips` report with `uid` as the `userID` of its
/// header or body object (see the module documentation). A text report names no one.
pub(crate) fn names_uid(text: &[u8], uid: u32) -> bool {
    if text.first() != Some(&b'{') {
        return false;
    }
    let values = serde_json::Deserializer::from_slice(text).into_iter::<serde_json::Value>();
    // A body cut off at the end of what was read is an error after the header.
    values.map_while(Result::ok).take(2).any(|value| {
        // Written as a plain decimal: a leading zero is another spelling, not the uid.
        value.get("userID").and_then(serde_json::Value::as_u64) == Some(u64::from(uid))
    })
}

/// What a walk does with a report that is the uid's.
trait Act {
    /// Does it to `name` in `dir`, which is at `path` (for messages).
    fn on(&mut self, dir: &OwnedFd, name: &CString, path: &Path) -> std::io::Result<()>;
}

/// Removes it, through the folder's descriptor.
struct Remove;

impl Act for Remove {
    fn on(&mut self, dir: &OwnedFd, name: &CString, _: &Path) -> std::io::Result<()> {
        Ok(rustix::fs::unlinkat(
            dir,
            name.as_c_str(),
            AtFlags::empty(),
        )?)
    }
}

/// Notes it, and removes nothing.
struct Note<'a>(&'a mut Vec<String>);

impl Act for Note<'_> {
    fn on(&mut self, _: &OwnedFd, _: &CString, path: &Path) -> std::io::Result<()> {
        self.0.push(path.display().to_string());
        Ok(())
    }
}

/// The bytes still to be read in one sweep or check.
struct Budget(usize);

impl Budget {
    /// Takes up to `bytes`; how many.
    fn grant(&mut self, bytes: usize) -> usize {
        let given = bytes.min(self.0);
        self.0 -= given;
        given
    }

    /// Gives back what was granted and not used.
    fn refund(&mut self, bytes: usize) {
        self.0 += bytes;
    }
}

/// Removes the uid's reports from `dirs` (absolute paths, walked from `/`); the failures,
/// each with its path.
pub(crate) fn sweep(dirs: &[PathBuf], scope: &Scope) -> Vec<String> {
    from_root(dirs, scope, &mut Remove)
}

/// The uid's reports in `dirs` that a sweep would remove: what the clean's check looks
/// at, without removing it.
pub(crate) fn found(dirs: &[PathBuf], scope: &Scope) -> Vec<String> {
    let mut found = Vec::new();
    let errors = from_root(dirs, scope, &mut Note(&mut found));
    found.extend(errors);
    found
}

/// [`each`] with the whole [`READ_BUDGET`] over absolute `dirs`, from a descriptor on `/`.
fn from_root(dirs: &[PathBuf], scope: &Scope, act: &mut dyn Act) -> Vec<String> {
    let root = rustix::fs::openat(CWD, "/", DIRECTORY, Mode::empty());
    match root {
        Ok(root) => each(&root, dirs, scope, READ_BUDGET, act),
        Err(why) => vec![format!("/: {why}")],
    }
}

/// Calls `act` on each entry of `dirs` (paths from the folder `root` is a descriptor on,
/// absolute ones with their `/` taken off) that is the uid's. Each failure of a folder
/// the clean answers for is returned with its path, and the rest goes on.
fn each(
    root: &OwnedFd,
    dirs: &[PathBuf],
    scope: &Scope,
    reads: usize,
    act: &mut dyn Act,
) -> Vec<String> {
    let mut errors = Vec::new();
    let mut budget = Budget(reads);
    for dir in dirs {
        let relative = dir.strip_prefix("/").unwrap_or(dir);
        let Some(folder) = open_folder(root, relative, scope) else {
            continue;
        };
        walk(&folder, dir, scope, (&mut budget, act), &mut errors);
    }
    errors
}

/// The folder `relative` below `root`, every component opened without following a link;
/// `None` (and a debug line) when a component is not there, is a link or cannot be
/// opened, or when a directory on the way is a pool user's.
fn open_folder(root: &OwnedFd, relative: &Path, scope: &Scope) -> Option<OwnedFd> {
    let mut dir = rustix::fs::openat(root, ".", DIRECTORY, Mode::empty()).ok()?;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return None;
        };
        let next = rustix::fs::openat(&dir, name, DIRECTORY, Mode::empty());
        let Ok(next) = next else {
            tracing::debug!(folder = %relative.display(), "crash folder not walked: not a directory here");
            return None;
        };
        if scope.pool.contains(&rustix::fs::fstat(&next).ok()?.st_uid) {
            tracing::debug!(folder = %relative.display(), "crash folder not walked: a pool user's directory is on its path");
            return None;
        }
        dir = next;
    }
    Some(dir)
}

/// The entries of the opened `folder` (at `path`).
fn walk(
    folder: &OwnedFd,
    path: &Path,
    scope: &Scope,
    (budget, act): (&mut Budget, &mut dyn Act),
    errors: &mut Vec<String>,
) {
    // A folder of another user's: what goes wrong in it is not this clean's to answer for.
    let owner = rustix::fs::fstat(folder).map_or(u32::MAX, |s| s.st_uid);
    let answerable = answers_for(owner, scope.uid, rustix::process::geteuid().as_raw());
    let mut fail = |what: String| note_failure(errors, answerable, path, what);
    let entries = match Dir::read_from(folder) {
        Ok(entries) => entries,
        Err(why) => return fail(format!("{}: {why}", path.display())),
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_owned();
        if name.as_bytes() == b"." || name.as_bytes() == b".." {
            continue;
        }
        // Gone since the listing: nothing to do.
        let Ok(stat) = rustix::fs::statat(folder, name.as_c_str(), AtFlags::SYMLINK_NOFOLLOW)
        else {
            continue;
        };
        if !hers(folder, &name, &stat, scope.uid, budget) {
            continue;
        }
        let entry_path = path.join(name.to_string_lossy().as_ref());
        match act.on(folder, &name, &entry_path) {
            // Gone since the listing: removed already.
            Err(why) if why.kind() != std::io::ErrorKind::NotFound => {
                fail(format!("{}: {why}", entry_path.display()));
            }
            _ => {}
        }
    }
}

/// Whether the clean answers for the failures in a folder `owner` owns: its own folders
/// (root's, the slot's uid's, the daemon's), not another user's.
fn answers_for(owner: u32, uid: u32, daemon: u32) -> bool {
    owner == 0 || owner == uid || owner == daemon
}

/// Records `what` as a failure, or, when the clean does not answer for the folder, only
/// logs it.
fn note_failure(errors: &mut Vec<String>, answerable: bool, folder: &Path, what: String) {
    if answerable {
        errors.push(what);
    } else {
        tracing::debug!(folder = %folder.display(), "{what}: skipped, not this slot's folder");
    }
}

#[allow(clippy::unnecessary_cast)] // `st_mode` is u32 on Linux, u16 on macOS
fn kind(stat: &Stat) -> FileType {
    FileType::from_raw_mode(stat.st_mode as _)
}

/// Whether the entry `name` of `folder`, examined as `stat`, is the uid's: owned by it
/// (anything but a directory), or a regular file that [`names_uid`] says is.
fn hers(folder: &OwnedFd, name: &CString, stat: &Stat, uid: u32, budget: &mut Budget) -> bool {
    match kind(stat) {
        FileType::Directory => false,
        FileType::RegularFile if stat.st_uid != uid => names(folder, name, stat, uid, budget),
        _ => stat.st_uid == uid,
    }
}

/// Whether the regular file `name` of `folder` (examined as `stat`) is a report that
/// names `uid`: opened on the folder's descriptor without following a link or blocking,
/// checked to be the file examined, and read within `budget`. False when it cannot be.
fn names(folder: &OwnedFd, name: &CString, stat: &Stat, uid: u32, budget: &mut Budget) -> bool {
    let Ok(file) = rustix::fs::openat(folder, name.as_c_str(), FILE, Mode::empty()) else {
        return false;
    };
    let Ok(opened) = rustix::fs::fstat(&file) else {
        return false;
    };
    // Swapped for something else since it was examined: not read.
    if (opened.st_dev, opened.st_ino) != (stat.st_dev, stat.st_ino)
        || kind(&opened) != FileType::RegularFile
    {
        return false;
    }
    let allowed = budget.grant(HEAD);
    if allowed < HEAD {
        tracing::warn!("crash reports: the read budget is spent; a report is left unread");
        budget.refund(allowed);
        return false;
    }
    let mut text = Vec::new();
    // A read that fails part way leaves what was read so far.
    let mut head = std::fs::File::from(file).take(HEAD as u64);
    let _ = head.read_to_end(&mut text);
    budget.refund(HEAD - text.len());
    // Only a JSON report is read whole, and within what is left of the budget.
    if text.first() == Some(&b'{') {
        let more = budget.grant(WHOLE - HEAD);
        let before = text.len();
        let _ = head.into_inner().take(more as u64).read_to_end(&mut text);
        budget.refund(more - (text.len() - before));
    }
    names_uid(&text, uid)
}

#[cfg(test)]
mod tests;
