//! C9 of the clean: the crash reports about the slot's uid. A process of a pool user that
//! crashes leaves a report outside the slot's home: in the system's crash folder, in the
//! console user's own, or a core file in `/cores`. The next lease would read it, and a
//! report names the crashed program's arguments and paths.
//!
//! A report is the uid's when the file is **owned by the uid**, or when its **header names
//! the uid** ([`names_uid`]): the `userID` key of an `.ips` report's header or body
//! object, or the `User ID:` line of a text report's header, never a number quoted in the
//! body. That is the whole attribution rule. How
//! `ReportCrash` attributes a report to a user when the crashed process is not the
//! console user's, and where it writes one on a Mac with no console user, is not
//! measured (experiments X6 and G18 of the Mac pool plan): a report this rule cannot
//! attribute stays, and a test that plants a different shape does not exist yet.

use std::fs;
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

/// How much of a text report [`names_uid`] reads: its header is in the first lines.
pub(crate) const HEAD: usize = 16 * 1024;

/// How much of an `.ips` report is read: both of its JSON values must be whole to be
/// parsed, and a report larger than this names no one.
pub(crate) const WHOLE: usize = 4 * 1024 * 1024;

/// The sections of a text report that end its header: a line that starts one.
const BODY: [&str; 5] = [
    "Thread ",
    "Backtrace",
    "Last Exception Backtrace",
    "Application Specific Information:",
    "Binary Images:",
];

/// Whether the crash report `text` names `uid` as the user of the crashed process, in its
/// **header** and nowhere else (a report owned by another user can quote any number in
/// its body, and the clean removes what this finds as root):
///
/// - an `.ips` report (it starts with `{`) is two JSON values, a header line and a body;
///   `userID` must be a key of one of those two objects, not of anything nested in them;
/// - a text report names it in a line that starts with `User ID:`, before the first
///   section of the body ([`BODY`]).
///
/// The whole number must equal the uid.
pub(crate) fn names_uid(text: &[u8], uid: u32) -> bool {
    if text.first() == Some(&b'{') {
        let values = serde_json::Deserializer::from_slice(text).into_iter::<serde_json::Value>();
        // A body cut off at the end of what was read is an error after the header.
        return values.map_while(Result::ok).take(2).any(|value| {
            value.get("userID").and_then(serde_json::Value::as_u64) == Some(u64::from(uid))
        });
    }
    let head = &text[..text.len().min(HEAD)];
    let text = String::from_utf8_lossy(head);
    for line in text.lines() {
        if BODY.iter().any(|section| line.starts_with(section)) {
            return false;
        }
        if let Some(value) = line.strip_prefix("User ID:") {
            return starts_with_number(value.trim_start(), uid);
        }
    }
    false
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
        let listed = entries.filter_map(Result::ok).filter_map(|entry| {
            let path = entry.path();
            // Gone since the listing: nothing to do.
            fs::symlink_metadata(&path).ok().map(|meta| (path, meta))
        });
        for (path, meta) in listed {
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

/// Whether the regular file `path` is a report that names `uid` ([`names_uid`]); false
/// when it cannot be read.
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
    let mut text = Vec::new();
    // A read that fails part way leaves what was read so far.
    let mut head = file.take(HEAD as u64);
    let _ = head.read_to_end(&mut text);
    // Only a JSON report is read whole: a core file or a text report is judged by its head.
    if text.first() == Some(&b'{') {
        let _ = head
            .into_inner()
            .take((WHOLE - HEAD) as u64)
            .read_to_end(&mut text);
    }
    names_uid(&text, uid)
}

#[cfg(test)]
mod tests;
