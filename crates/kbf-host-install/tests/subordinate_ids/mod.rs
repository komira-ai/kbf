//! The subordinate ids the rehearsal gives the daemon's account for rootless containers, in
//! `/etc/subuid` and `/etc/subgid`, and the guard that takes them out again.

use std::fs;
use std::path::{Path, PathBuf};

/// The lines the rehearsal appended to subordinate-id files, taken out again on drop.
///
/// Each file's entry is recorded before the file is written, so a failure part way (the
/// second file cannot be written) still takes the first file's line out as the guard unwinds.
/// Only the appended text is taken out: whatever else was written to a file meanwhile stays.
pub struct SubIds {
    /// Each file, with the exact text appended to it.
    added: Vec<Appended>,
}

/// One file's change: the text appended to it, and whether the file was there before.
struct Appended {
    file: PathBuf,
    text: String,
    existed: bool,
}

impl SubIds {
    /// Gives `account` a range of subordinate ids in each of `files` that has none for it
    /// (see [`sub_id_line`]); a file that does not exist is made with the one line, and removed
    /// again on drop.
    pub fn add(files: &[&Path], account: &str) -> std::io::Result<Self> {
        let mut guard = Self { added: Vec::new() };
        for file in files {
            let (before, existed) = match fs::read_to_string(file) {
                Ok(text) => (text, true),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (String::new(), false),
                Err(e) => return Err(e),
            };
            let Some(line) = sub_id_line(&before, account) else {
                continue;
            };
            let gap = if before.is_empty() || before.ends_with('\n') {
                ""
            } else {
                "\n"
            };
            let appended = format!("{gap}{line}\n");
            guard.added.push(Appended {
                file: file.to_path_buf(),
                text: appended.clone(),
                existed,
            });
            fs::write(file, before + &appended)?;
        }
        Ok(guard)
    }
}

impl Drop for SubIds {
    fn drop(&mut self) {
        for Appended {
            file,
            text: appended,
            existed,
        } in &self.added
        {
            let Ok(text) = fs::read_to_string(file) else {
                continue;
            };
            match without_appended(&text, appended) {
                // A file the guard made, holding nothing else: gone again.
                Some(after) if after.is_empty() && !existed => {
                    let _ = fs::remove_file(file);
                }
                Some(after) => {
                    let _ = fs::write(file, after);
                }
                None => {}
            }
        }
    }
}

/// `text` without the `appended` text: cut off its end when the file still ends with it,
/// else its line taken out where it is; `None` when the line is not there (it was never
/// written, or was taken out already).
fn without_appended(text: &str, appended: &str) -> Option<String> {
    if let Some(before) = text.strip_suffix(appended) {
        return Some(before.to_owned());
    }
    let line = appended.trim_start_matches('\n');
    text.find(line)
        .filter(|at| *at == 0 || text.as_bytes()[at - 1] == b'\n')
        .map(|at| format!("{}{}", &text[..at], &text[at + line.len()..]))
}

/// The ids one account gets: a container's whole 16-bit range.
const SUB_ID_COUNT: u64 = 65536;

/// The line that gives `account` a range of subordinate ids in a file holding `existing`
/// (`/etc/subuid` or `/etc/subgid`: `<name>:<first>:<count>` per line), or `None` when the
/// account has a range already. The range starts at 100000 or above every range the file
/// gives, on a multiple of [`SUB_ID_COUNT`], so it overlaps none.
fn sub_id_line(existing: &str, account: &str) -> Option<String> {
    let ranges: Vec<(&str, u64, u64)> = existing
        .lines()
        .filter_map(|line| {
            let mut parts = line.trim().splitn(3, ':');
            let (name, first, count) = (parts.next()?, parts.next()?, parts.next()?);
            Some((name, first.parse().ok()?, count.parse().ok()?))
        })
        .collect();
    if ranges.iter().any(|(name, _, _)| *name == account) {
        return None;
    }
    let above = ranges
        .iter()
        .map(|(_, first, count)| first.saturating_add(*count))
        .fold(100_000, u64::max);
    let first = above.div_ceil(SUB_ID_COUNT) * SUB_ID_COUNT;
    Some(format!("{account}:{first}:{SUB_ID_COUNT}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The service account's subordinate ids: a range above every range the file gives, on a
    /// boundary of 65536 and at 100000 or above, so it overlaps none; nothing when the account has
    /// one; lines that are not ranges are passed over.
    #[test]
    fn the_subordinate_ids_overlap_no_range_the_file_gives() {
        assert_eq!(
            sub_id_line("", "kbf-rehearsal").as_deref(),
            Some("kbf-rehearsal:131072:65536")
        );
        assert_eq!(
            sub_id_line("runner:100000:65536\n", "kbf-rehearsal").as_deref(),
            Some("kbf-rehearsal:196608:65536")
        );
        assert_eq!(
            sub_id_line(
                "# a comment\nrunner:100000:65536\nbuilder:231072:65536\nbroken:x:1\n",
                "kbf-rehearsal"
            )
            .as_deref(),
            Some("kbf-rehearsal:327680:65536")
        );
        assert_eq!(
            sub_id_line(
                "runner:100000:65536\nkbf-rehearsal:500000:1000\n",
                "kbf-rehearsal"
            ),
            None
        );
    }

    /// The subordinate-id files come back exactly as they were when the guard is dropped: with and
    /// without a last newline, and a file the guard made is removed again.
    #[test]
    fn the_subordinate_id_files_come_back_as_they_were() {
        let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "subids-restore");
        let uid = dir.path().join("subuid");
        let gid = dir.path().join("subgid");
        let made = dir.path().join("made");
        fs::write(&uid, "runner:100000:65536\n").unwrap();
        fs::write(&gid, "runner:100000:65536").unwrap();
        let guard = SubIds::add(&[&uid, &gid, &made], "kbf-rehearsal").unwrap();
        assert_eq!(
            fs::read_to_string(&uid).unwrap(),
            "runner:100000:65536\nkbf-rehearsal:196608:65536\n"
        );
        assert_eq!(
            fs::read_to_string(&gid).unwrap(),
            "runner:100000:65536\nkbf-rehearsal:196608:65536\n"
        );
        assert_eq!(
            fs::read_to_string(&made).unwrap(),
            "kbf-rehearsal:131072:65536\n"
        );
        drop(guard);
        assert_eq!(fs::read_to_string(&uid).unwrap(), "runner:100000:65536\n");
        assert_eq!(fs::read_to_string(&gid).unwrap(), "runner:100000:65536");
        assert!(!made.exists());
    }

    /// When the second file cannot be written, the first file's line is taken out again before the
    /// error is returned: the first file is exactly as it was.
    #[test]
    fn a_failure_on_the_second_file_leaves_the_first_as_it_was() {
        let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "subids-fail");
        let uid = dir.path().join("subuid");
        let before = "# ids\nrunner:100000:65536\n";
        fs::write(&uid, before).unwrap();
        // A file in a directory that does not exist: it reads as empty, and its write fails.
        let gid = dir.path().join("no-such-dir").join("subgid");
        let refused = SubIds::add(&[&uid, &gid], "kbf-rehearsal").err();
        assert_eq!(
            refused.map(|e| e.kind()),
            Some(std::io::ErrorKind::NotFound)
        );
        assert_eq!(fs::read_to_string(&uid).unwrap(), before);
        assert!(!gid.exists());
    }

    /// A file that cannot be read stops the guard before it writes anything, and a file that
    /// already gives the account a range is left alone.
    #[test]
    fn an_unreadable_file_or_an_account_with_ids_writes_nothing() {
        let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "subids-unread");
        let uid = dir.path().join("subuid");
        let before = "runner:100000:65536\n";
        fs::write(&uid, before).unwrap();
        // A directory reads as an error other than "not found".
        let gid = dir.path().join("subgid");
        fs::create_dir(&gid).unwrap();
        assert!(SubIds::add(&[&gid, &uid], "kbf-rehearsal").is_err());
        assert_eq!(fs::read_to_string(&uid).unwrap(), before);

        let has = "kbf-rehearsal:500000:65536\n";
        fs::write(&uid, has).unwrap();
        let guard = SubIds::add(&[&uid], "kbf-rehearsal").unwrap();
        assert_eq!(fs::read_to_string(&uid).unwrap(), has);
        drop(guard);
        assert_eq!(fs::read_to_string(&uid).unwrap(), has);
    }

    /// Lines written to a file while the guard held it stay when it is dropped: only the
    /// rehearsal's own line is taken out, wherever it now is; a line taken out already is left so.
    #[test]
    fn only_the_rehearsals_own_line_is_taken_out() {
        let ours = "\nkbf-rehearsal:196608:65536\n";
        assert_eq!(
            without_appended(
                "runner:100000:65536\nkbf-rehearsal:196608:65536\nother:262144:65536\n",
                ours
            )
            .as_deref(),
            Some("runner:100000:65536\nother:262144:65536\n")
        );
        // Still at the end: cut off with the newline it was given, as the file was.
        assert_eq!(
            without_appended("runner:100000:65536\nkbf-rehearsal:196608:65536\n", ours).as_deref(),
            Some("runner:100000:65536")
        );
        assert_eq!(
            without_appended("runner:100000:65536\nx-kbf-rehearsal:196608:65536\n", ours),
            None
        );
        assert_eq!(without_appended("runner:100000:65536\n", ours), None);

        let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "subids-other");
        let uid = dir.path().join("subuid");
        fs::write(&uid, "runner:100000:65536\n").unwrap();
        let guard = SubIds::add(&[&uid], "kbf-rehearsal").unwrap();
        let mut now = fs::read_to_string(&uid).unwrap();
        now.push_str("other:262144:65536\n");
        fs::write(&uid, now).unwrap();
        drop(guard);
        assert_eq!(
            fs::read_to_string(&uid).unwrap(),
            "runner:100000:65536\nother:262144:65536\n"
        );
    }
}
