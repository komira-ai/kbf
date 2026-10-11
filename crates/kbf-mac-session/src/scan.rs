//! The leak scan's items (S4.2 `scan` and `baseline`; fleet-updates.md 10.2, layer L3),
//! the minimal set: the launch agent and daemon folders, launchd's login items,
//! Background Task Management's entries, users in the lease range, and the auto-login
//! state. fleet-updates.md 10.2 lists more items; they are not collected yet.
//!
//! Each item is one line of text, `<class> <what>`, so a baseline is a sorted list of
//! lines and a scan's result is what was added to it and what was removed:
//!
//! - `launch-agents`, `launch-daemons`: each entry of `/Library/LaunchAgents` and
//!   `/Library/LaunchDaemons`, with its kind, owner and mode, and a file's SHA-256 (a
//!   link's target, never followed).
//! - `login-items`: the same for launchd's `loginitems.<uid>.plist` files in
//!   `/private/var/db/com.apple.xpc.launchd`.
//! - `background-tasks`: each entry `sfltool dumpbtm` prints, by the uid its record
//!   is under, its identifier, type, URL, executable and disposition.
//! - `lease-users`: each uid of the lease range that a user record has.
//! - `auto-login`: the login window's `autoLoginUser`, whether `/etc/kcpassword` is
//!   there, and the session switch's intent record.
//!
//! An item that cannot be read is an error, never an empty class: a scan with an
//! error is not clean.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::io::{self, Read as _};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags};
use sha2::{Digest as _, Sha256};

use crate::autologin::{AUTO_LOGIN_USER, DEFAULTS, read_intent};
use crate::helper::{Host, Settings};
use crate::sweep::list;

/// The program that prints Background Task Management's entries.
pub const SFLTOOL: &str = "/usr/bin/sfltool";

/// The first line of a baseline file.
pub const BASELINE_HEADER: &str = "kbf-scan-baseline-v1";

pub const LAUNCH_AGENTS: &str = "launch-agents";
pub const LAUNCH_DAEMONS: &str = "launch-daemons";
pub const LOGIN_ITEMS: &str = "login-items";
pub const BACKGROUND_TASKS: &str = "background-tasks";
pub const LEASE_USERS: &str = "lease-users";
pub const AUTO_LOGIN: &str = "auto-login";

const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// A folder whose entries are scan items.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanFolder {
    /// The items' class.
    pub class: &'static str,
    pub path: PathBuf,
    /// Only entries whose names start with this, if set.
    pub prefix: Option<&'static str>,
}

impl ScanFolder {
    /// macOS's folders of the minimal item set.
    #[must_use]
    pub fn macos() -> Vec<Self> {
        vec![
            Self {
                class: LAUNCH_AGENTS,
                path: PathBuf::from("/Library/LaunchAgents"),
                prefix: None,
            },
            Self {
                class: LAUNCH_DAEMONS,
                path: PathBuf::from("/Library/LaunchDaemons"),
                prefix: None,
            },
            Self {
                class: LOGIN_ITEMS,
                path: PathBuf::from("/private/var/db/com.apple.xpc.launchd"),
                prefix: Some("loginitems."),
            },
        ]
    }
}

/// Where the scan looks and keeps its state.
#[derive(Clone, Debug)]
pub struct ScanSettings {
    pub folders: Vec<ScanFolder>,
    /// The recorded baseline (in the state directory).
    pub baseline: PathBuf,
    /// The baseline window: written by the root tool that applied a software set or
    /// erased the Mac, it names the boot it was written in (one line). `baseline` is
    /// accepted only while it exists and names another boot; the first `user-create`
    /// removes it.
    pub window: PathBuf,
}

impl ScanSettings {
    /// What a test scans: the three folders of [`ScanFolder::macos`] under `dir`
    /// (`LaunchAgents`, `LaunchDaemons`, `launchd`), the state files in it.
    #[cfg(test)]
    pub(crate) fn under(dir: &Path) -> Self {
        let folders = ScanFolder::macos()
            .into_iter()
            .zip(["LaunchAgents", "LaunchDaemons", "launchd"])
            .map(|(folder, name)| ScanFolder {
                path: dir.join(name),
                ..folder
            })
            .collect();
        Self::in_state_dir(dir, folders)
    }

    /// The files in the state directory `dir`, and `folders`.
    #[must_use]
    pub fn in_state_dir(dir: &Path, folders: Vec<ScanFolder>) -> Self {
        Self {
            folders,
            baseline: dir.join("scan-baseline"),
            window: dir.join("baseline-window"),
        }
    }
}

/// What one collection found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub items: BTreeSet<String>,
    /// What could not be read, and why.
    pub errors: Vec<String>,
}

impl Snapshot {
    /// How many items each class holds, every class of the set named.
    #[must_use]
    pub fn classes(&self, folders: &[ScanFolder]) -> Vec<(String, usize)> {
        let mut counts: BTreeMap<&str, usize> = folders
            .iter()
            .map(|folder| folder.class)
            .chain([BACKGROUND_TASKS, LEASE_USERS, AUTO_LOGIN])
            .map(|class| (class, 0))
            .collect();
        for item in &self.items {
            let class = item.split(' ').next().unwrap_or_default();
            *counts.entry(class).or_default() += 1;
        }
        counts
            .into_iter()
            .map(|(class, n)| (class.to_owned(), n))
            .collect()
    }
}

/// What a scan found against the baseline.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Diff {
    /// Items the baseline does not hold.
    pub added: Vec<String>,
    /// Baseline items no longer found.
    pub removed: Vec<String>,
}

/// `now` against `baseline`.
#[must_use]
pub fn diff(baseline: &BTreeSet<String>, now: &BTreeSet<String>) -> Diff {
    Diff {
        added: now.difference(baseline).cloned().collect(),
        removed: baseline.difference(now).cloned().collect(),
    }
}

/// Collects every item of the minimal set.
#[must_use]
pub fn collect(host: &dyn Host, settings: &Settings) -> Snapshot {
    let mut snapshot = Snapshot::default();
    for folder in &settings.scan.folders {
        if let Err(why) = folder_items(folder, &mut snapshot.items) {
            snapshot
                .errors
                .push(format!("{} {}: {why}", folder.class, folder.path.display()));
        }
    }
    let collected = [
        background_tasks(host),
        lease_users(host, settings),
        auto_login(host, settings),
    ];
    for result in collected {
        match result {
            Ok(items) => snapshot.items.extend(items),
            Err(why) => snapshot.errors.push(why),
        }
    }
    snapshot
}

/// The entries of `folder`, never following a link; none if it does not exist.
fn folder_items(folder: &ScanFolder, items: &mut BTreeSet<String>) -> io::Result<()> {
    let dir = match rustix::fs::openat(CWD, &folder.path, DIRECTORY, Mode::empty()) {
        Err(rustix::io::Errno::NOENT) => return Ok(()),
        other => other?,
    };
    for name in list(&dir)? {
        let wanted = folder
            .prefix
            .is_none_or(|prefix| name.as_bytes().starts_with(prefix.as_bytes()));
        if wanted {
            let what = entry(&dir, &name)
                .map_err(|why| io::Error::new(why.kind(), format!("{}: {why}", name.display())))?;
            items.insert(format!("{} {name:?} {what}", folder.class));
        }
    }
    Ok(())
}

/// One entry: its kind, owner and mode, and a file's hash or a link's target.
fn entry(dir: &OwnedFd, name: &OsStr) -> io::Result<String> {
    let stat = rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
    let owner = format!(
        "{}:{} {:o}",
        stat.st_uid,
        stat.st_gid,
        stat.st_mode & 0o7777
    );
    Ok(match FileType::from_raw_mode(stat.st_mode as _) {
        FileType::RegularFile => format!("file {owner} sha256:{}", hash_file(dir, name)?),
        FileType::Symlink => {
            let target = rustix::fs::readlinkat(dir, name, Vec::new())?;
            format!("link {owner} -> {:?}", OsStr::from_bytes(target.as_bytes()))
        }
        FileType::Directory => format!("dir {owner}"),
        _ => format!("other {owner}"),
    })
}

/// The SHA-256 of file `name` in `dir`, opened without following a link or blocking:
/// a FIFO swapped in since it was examined reads as empty, it is not waited on (and
/// the hash then differs from the baseline's, which is a finding).
fn hash_file(dir: &OwnedFd, name: &OsStr) -> io::Result<String> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let mut file = std::fs::File::from(rustix::fs::openat(dir, name, flags, Mode::empty())?);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Background Task Management's entries, from `sfltool dumpbtm`.
fn background_tasks(host: &dyn Host) -> Result<Vec<String>, String> {
    let output = host
        .command(SFLTOOL, &["dumpbtm"])
        .map_err(|why| format!("{BACKGROUND_TASKS}: running {SFLTOOL}: {why}"))?;
    if !output.success {
        return Err(format!(
            "{BACKGROUND_TASKS}: {SFLTOOL} dumpbtm failed: {}",
            output.stderr.trim()
        ));
    }
    parse_dumpbtm(&output.stdout).map_err(|why| format!("{BACKGROUND_TASKS}: {why}"))
}

/// The fields of a `dumpbtm` entry that make its item.
const BTM_FIELDS: [(&str, &str); 5] = [
    ("Identifier", "id"),
    ("Type", "type"),
    ("URL", "url"),
    ("Executable Path", "exec"),
    ("Disposition", "disposition"),
];

/// Parses `sfltool dumpbtm`: records headed `Records for UID <uid> : <uuid>`, each
/// holding entries headed `#<n>:` alone on a line, each followed by `<Field>: <value>`
/// lines. An embedded item's `#<n>: <identifier>` line carries a value, so it is a
/// field, not an entry. Fields other than [`BTM_FIELDS`] (the entry's UUID, its
/// generation) change without anything being installed, so they are left out.
///
/// # Errors
/// The text is not empty but has no record header, or an entry comes before one.
pub fn parse_dumpbtm(text: &str) -> Result<Vec<String>, String> {
    let mut items = Vec::new();
    let mut uid: Option<String> = None;
    let mut entry: Option<BTreeMap<&str, String>> = None;
    let finish = |uid: &Option<String>, entry: Option<BTreeMap<&str, String>>| {
        entry.map(|fields| {
            let mut item = format!("{BACKGROUND_TASKS} uid={}", uid.as_deref().unwrap_or("?"));
            for (field, key) in BTM_FIELDS {
                let value = fields.get(field).map_or("", String::as_str);
                // Writing to a String cannot fail.
                let _ = write!(item, " {key}={value:?}");
            }
            item
        })
    };
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Records for UID ") {
            items.extend(finish(&uid, entry.take()));
            let id = rest.split_whitespace().next().unwrap_or_default();
            uid = Some(id.to_owned());
        } else if is_entry_header(line) {
            if uid.is_none() {
                return Err(format!("an entry ({line}) before any record header"));
            }
            items.extend(finish(&uid, entry.take()));
            entry = Some(BTreeMap::new());
        } else if let (Some(fields), Some((field, value))) = (entry.as_mut(), line.split_once(':'))
        {
            fields
                .entry(field.trim())
                .or_insert_with(|| value.trim().to_owned());
        }
    }
    items.extend(finish(&uid, entry.take()));
    if uid.is_none() && !text.trim().is_empty() {
        return Err("no \"Records for UID\" header in sfltool's output".to_owned());
    }
    Ok(items)
}

/// `#<digits>:` and nothing else.
fn is_entry_header(line: &str) -> bool {
    line.strip_prefix('#')
        .and_then(|rest| rest.strip_suffix(':'))
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

/// Each uid of the lease range a user record has.
fn lease_users(host: &dyn Host, settings: &Settings) -> Result<Vec<String>, String> {
    let mut items = Vec::new();
    for uid in settings.range.after(None) {
        let taken = host
            .uid_taken(uid)
            .map_err(|why| format!("{LEASE_USERS}: looking up uid {uid}: {why}"))?;
        if taken {
            items.push(format!("{LEASE_USERS} uid {uid}"));
        }
    }
    Ok(items)
}

/// The login window's `autoLoginUser`, `/etc/kcpassword`, and the intent record.
fn auto_login(host: &dyn Host, settings: &Settings) -> Result<Vec<String>, String> {
    let paths = &settings.session;
    let mut items = Vec::new();
    let domain = paths.loginwindow.as_str();
    let read = host
        .command(DEFAULTS, &["read", domain, AUTO_LOGIN_USER])
        .map_err(|why| format!("{AUTO_LOGIN}: running {DEFAULTS}: {why}"))?;
    if read.success {
        items.push(format!("{AUTO_LOGIN} user {:?}", read.stdout.trim()));
    } else if !read.stderr.contains("does not exist") {
        return Err(format!(
            "{AUTO_LOGIN}: defaults read {domain} {AUTO_LOGIN_USER} failed: {}",
            read.stderr.trim()
        ));
    }
    match rustix::fs::statat(CWD, &paths.kcpassword, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => items.push(format!("{AUTO_LOGIN} kcpassword present")),
        Err(rustix::io::Errno::NOENT) => {}
        Err(why) => {
            return Err(format!(
                "{AUTO_LOGIN}: {}: {why}",
                paths.kcpassword.display()
            ));
        }
    }
    match read_intent(&paths.intent) {
        Ok(Some(lease)) => items.push(format!("{AUTO_LOGIN} intent {lease}")),
        Ok(None) => {}
        Err(why) => return Err(format!("{AUTO_LOGIN}: {why}")),
    }
    Ok(items)
}

/// Writes the baseline, whole and durably.
///
/// # Errors
/// It could not be written.
pub fn write_baseline(path: &Path, items: &BTreeSet<String>) -> io::Result<()> {
    let mut text = format!("{BASELINE_HEADER}\n");
    for item in items {
        text.push_str(item);
        text.push('\n');
    }
    crate::autologin::write_whole(path, text.as_bytes())
}

/// The recorded baseline; `None` if none was recorded.
///
/// # Errors
/// It cannot be read, is not the helper's own, or does not start with the header.
pub fn read_baseline(path: &Path) -> Result<Option<BTreeSet<String>>, String> {
    let Some(text) = read_owned(path)? else {
        return Ok(None);
    };
    let mut lines = text.lines();
    if lines.next() != Some(BASELINE_HEADER) {
        return Err(format!(
            "{}: not a baseline ({BASELINE_HEADER} missing)",
            path.display()
        ));
    }
    Ok(Some(lines.map(str::to_owned).collect()))
}

/// The boot the baseline window was opened in; `None` if it is closed.
///
/// # Errors
/// It cannot be read, is not the helper's own, or is not one non-empty line.
pub fn read_window(path: &Path) -> Result<Option<String>, String> {
    let Some(text) = read_owned(path)? else {
        return Ok(None);
    };
    match text.strip_suffix('\n') {
        Some(boot) if !boot.is_empty() && !boot.contains('\n') => Ok(Some(boot.to_owned())),
        _ => Err(format!(
            "{}: {text:?} does not name one boot",
            path.display()
        )),
    }
}

/// Reads a file of the state directory the helper trusts (see
/// [`read_trusted`](crate::start::read_trusted)); `None` if there is none.
fn read_owned(path: &Path) -> Result<Option<String>, String> {
    match crate::start::read_trusted(path) {
        Ok(text) => Ok(Some(text)),
        Err(why) if why.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(why) => Err(format!("{}: {why}", path.display())),
    }
}

#[cfg(test)]
mod tests;
