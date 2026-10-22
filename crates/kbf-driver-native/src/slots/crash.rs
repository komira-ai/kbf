//! C9 of the clean: the crash reports about the slot's uid. A process of a pool user that
//! crashes leaves a report outside the slot's home: in the system's crash folder, in the
//! console user's own, or a core file in `/cores`. The next lease would read it, and a
//! report names the crashed program's arguments and paths.
//!
//! A report is the uid's when the file is **owned by the uid**, or when its **header names
//! the uid** ([`names_uid`]): the `userID` of an `.ips` report, or the `User ID:` line of
//! a text one, in the first [`HEAD`] bytes. That is the whole attribution rule. How
//! `ReportCrash` attributes a report to a user when the crashed process is not the
//! console user's, and where it writes one on a Mac with no console user, is not
//! measured (experiments X6 and G18 of the Mac pool plan): a report this rule cannot
//! attribute stays, and a test that plants a different shape does not exist yet.

use std::fs::{self, File};
use std::io::Read as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

/// The crash folders of the Mac itself, and the core file folder.
const SYSTEM: [&str; 4] = [
    "/Library/Logs/DiagnosticReports",
    "/Library/Logs/DiagnosticReports/Retired",
    "/cores",
    "/private/var/root/Library/Logs/DiagnosticReports",
];

/// Where a report about a pool user's process can be: [`SYSTEM`], and the
/// `DiagnosticReports` folder, and its `Retired` one, of every user in `/Users` (the
/// console user's crash reporter writes there, whoever crashed).
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

/// How much of a report [`names_uid`] reads: the header and the first lines of the body.
pub(crate) const HEAD: usize = 16 * 1024;

/// Whether the text `head` of a crash report names `uid` as the user of the crashed
/// process: a JSON `"userID": <uid>` (an `.ips` report) or a line `User ID: <uid>` (a
/// text report), the whole number, not a prefix of a longer one.
pub(crate) fn names_uid(head: &[u8], uid: u32) -> bool {
    let text = String::from_utf8_lossy(head);
    let json = text.match_indices("\"userID\"").any(|(at, key)| {
        let rest = text[at + key.len()..].trim_start();
        rest.strip_prefix(':')
            .is_some_and(|value| starts_with_number(value.trim_start(), uid))
    });
    json || text.lines().any(|line| {
        line.trim_start()
            .strip_prefix("User ID:")
            .is_some_and(|value| starts_with_number(value.trim_start(), uid))
    })
}

/// Whether `text` begins with exactly the decimal number `uid`: the digits end at a
/// character that is not a digit.
fn starts_with_number(text: &str, uid: u32) -> bool {
    let digits = text.len() - text.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    text[..digits].parse::<u32>().is_ok_and(|n| n == uid)
}

/// Calls `act` on each entry of `dirs` (a folder that is not there holds nothing) that is
/// the uid's by [the rule](self): owned by `uid`, or a regular file whose head names it.
/// Not looked into: a folder in a folder (`Retired` is listed on its own). Each failure,
/// of a listing or of `act`, is returned with its path, and the rest goes on.
fn each(
    dirs: &[PathBuf],
    uid: u32,
    mut act: impl FnMut(&Path) -> std::io::Result<()>,
) -> Vec<String> {
    let mut errors = Vec::new();
    for dir in dirs {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            // Not there, or under a file (`/Users/.localized` is one): it holds nothing.
            Err(why)
                if matches!(
                    why.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                continue;
            }
            Err(why) => {
                errors.push(format!("{}: {why}", dir.display()));
                continue;
            }
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            let hers =
                (!meta.is_dir() && meta.uid() == uid) || (meta.is_file() && names(&path, uid));
            if !hers {
                continue;
            }
            match act(&path) {
                // Gone since the listing: removed already.
                Err(why) if why.kind() != std::io::ErrorKind::NotFound => {
                    errors.push(format!("{}: {why}", path.display()));
                }
                _ => {}
            }
        }
    }
    errors
}

/// Removes the uid's reports from `dirs` ([`each`]); the failures, each with its path.
pub(crate) fn sweep(dirs: &[PathBuf], uid: u32) -> Vec<String> {
    each(dirs, uid, |path| fs::remove_file(path))
}

/// The uid's reports in `dirs` that a sweep would remove, and the folders that cannot
/// be read: what the clean's check looks at, without removing it.
pub(crate) fn found(dirs: &[PathBuf], uid: u32) -> Vec<String> {
    let mut found = Vec::new();
    let errors = each(dirs, uid, |path| {
        found.push(path.display().to_string());
        Ok(())
    });
    found.extend(errors);
    found
}

/// Whether the head of the regular file `path` names `uid`; false when it cannot be read.
fn names(path: &Path, uid: u32) -> bool {
    // Not following a link, not blocking: an entry swapped for a FIFO or a link since
    // the listing is read as nothing.
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path);
    let Ok(file) = file else {
        return false;
    };
    let mut head = Vec::new();
    let read = File::take(file, HEAD as u64).read_to_end(&mut head);
    read.is_ok() && names_uid(&head, uid)
}

#[cfg(test)]
mod tests;
