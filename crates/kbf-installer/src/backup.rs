//! Backup and restore of the state database (docs/design/frontend-proxy.md section 11): the
//! code behind `kbf-installer backup`, which `kbf-backup.service` (run by `kbf-backup.timer`)
//! calls, and `kbf-installer restore`.
//!
//! The only programs run are `pg_dump` and `pg_restore`, behind [`Tool`] so that everything
//! around them is tested without a server. A backup is one custom-format dump
//! (`pg_restore` reads it, and can restore a part of it):
//!
//! 1. the backup directory is made, or checked, and closed to everyone but its owner (0700);
//! 2. a temp file of an earlier run that crashed is removed;
//! 3. `pg_dump` writes the temp file, whose name is not a backup's;
//! 4. the file must be non-empty and `pg_restore --list` must read it (0600, fsync);
//! 5. a rename gives it its name, `kbf-<UTC time>.dump`, and the directory is synced: a backup
//!    is a whole file or it is not there;
//! 6. only then are the backups beyond the newest `backup_keep` (default 7) removed.
//!
//! Any failure removes the temp file, leaves every older backup alone and ends in an error,
//! which the unit turns into a failed state. The database is named by `installer.toml`'s
//! `database` key, which never carries a password, so the URL is safe in the command line;
//! no message here quotes it. Peer authentication means the verb runs as the database's role:
//! `kbf-server`, the user the unit runs as.

use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::database::check_url;
use crate::installer_toml::{DEFAULT_BACKUP_KEEP, InstallerToml};

/// The mode of the backup directory.
const DIR_MODE: u32 = 0o700;
/// The mode of a backup.
const FILE_MODE: u32 = 0o600;
/// How much of a program's standard error an error line carries.
const STDERR_TAIL: usize = 2000;
/// The most of `installer.toml` read (the closed reader's own cap, so one bound).
const MAX_FILE_BYTES: u64 = crate::installer_toml::MAX_FILE_BYTES as u64;

/// What a run of an external program can say.
pub trait Tool {
    /// Runs `program` with `args`, discarding its output.
    ///
    /// # Errors
    /// One line: the program could not start, or exited with a failure.
    fn run(&self, program: &str, args: &[OsString]) -> Result<(), String>;
}

/// The real programs: no standard input, no standard output kept, the end of standard error
/// in the error.
pub struct Process;

impl Tool for Process {
    fn run(&self, program: &str, args: &[OsString]) -> Result<(), String> {
        let out = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("cannot run {program}: {}", e.kind()))?;
        if out.status.success() {
            return Ok(());
        }
        let err = &out.stderr;
        let tail = String::from_utf8_lossy(&err[err.len().saturating_sub(STDERR_TAIL)..]);
        Err(format!(
            "{program} failed ({}): {}",
            out.status,
            tail.trim()
        ))
    }
}

/// What `installer.toml` says a backup needs.
#[derive(Debug, PartialEq, Eq)]
pub struct Settings {
    /// The `database` URL (no password).
    pub url: String,
    /// How many backups to keep.
    pub keep: u32,
}

impl Settings {
    /// Reads the installer's configuration at `path`: the closed reader, so a file `check`
    /// refuses is refused here too, and the URL held to the no-password rule.
    ///
    /// # Errors
    /// One line, never quoting the URL.
    pub fn read(path: &Path) -> Result<Settings, String> {
        let shown = path.display().to_string();
        let mut text = String::new();
        fs::File::open(path)
            .and_then(|f| f.take(MAX_FILE_BYTES + 1).read_to_string(&mut text))
            .map_err(|e| format!("{shown}: {}", e.kind()))?;
        let file = InstallerToml::parse(&text).map_err(|e| e.to_string())?;
        let Some(url) = file.database else {
            return Err(format!(
                "{shown} has no `database = \"postgres://...\"` setting"
            ));
        };
        check_url(&shown, &url).map_err(|e| e.to_string())?;
        Ok(Settings {
            url,
            keep: file.backup_keep.unwrap_or(DEFAULT_BACKUP_KEEP),
        })
    }
}

/// Where and how to dump.
pub struct Spec<'a> {
    /// The backup directory.
    pub dir: &'a Path,
    /// The database (no password).
    pub url: &'a str,
    /// How many backups stay, this one included (at least 1).
    pub keep: u32,
    /// The program that dumps.
    pub pg_dump: &'a str,
    /// The program that restores and lists.
    pub pg_restore: &'a str,
}

/// What a backup did.
#[derive(Debug)]
pub struct Done {
    /// The backup.
    pub file: PathBuf,
    /// Its size.
    pub bytes: u64,
    /// The names of the older backups removed, oldest first.
    pub removed: Vec<String>,
}

/// The name of a backup made at `secs` since the epoch: `kbf-YYYYMMDDTHHMMSSZ.dump`, UTC and
/// fixed width, so names sort as times.
#[must_use]
pub fn stamp(secs: u64) -> String {
    // Days since the epoch to a civil date (Howard Hinnant's `civil_from_days`).
    let days = i64::try_from(secs / 86_400).unwrap_or(i64::MAX / 2);
    let rest = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "kbf-{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z.dump",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// Whether `name` is a finished backup of this tool (what [`stamp`] writes).
#[must_use]
pub fn is_backup_name(name: &str) -> bool {
    let Some(time) = name
        .strip_prefix("kbf-")
        .and_then(|n| n.strip_suffix(".dump"))
    else {
        return false;
    };
    let b = time.as_bytes();
    b.len() == 16
        && b[..8].iter().all(u8::is_ascii_digit)
        && b[8] == b'T'
        && b[9..15].iter().all(u8::is_ascii_digit)
        && b[15] == b'Z'
}

/// The name the dump is written to before it is a backup.
fn temp_name(name: &str) -> String {
    format!(".{name}.tmp")
}

/// Whether `name` is the temp file of a run (finished or not).
fn is_temp_name(name: &str) -> bool {
    name.strip_prefix('.')
        .and_then(|n| n.strip_suffix(".tmp"))
        .is_some_and(is_backup_name)
}

fn words(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

/// Makes the backup directory (and its parents) or checks it: a real directory, ours, then
/// closed to everyone else.
fn prepare_dir(dir: &Path) -> Result<(), String> {
    let shown = dir.display();
    match fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() => {
            let me = rustix::process::geteuid().as_raw();
            if m.uid() != me {
                return Err(format!("{shown} is owned by another user"));
            }
        }
        Ok(_) => return Err(format!("{shown} is not a directory")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::DirBuilder::new()
            .recursive(true)
            .mode(DIR_MODE)
            .create(dir)
            .map_err(|e| format!("cannot make {shown}: {}", e.kind()))?,
        Err(e) => return Err(format!("{shown}: {}", e.kind())),
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(DIR_MODE))
        .map_err(|e| format!("cannot close {shown} to other users: {}", e.kind()))
}

/// The names in `dir` for which `keep` says yes, sorted.
fn names_where(dir: &Path, keep: fn(&str) -> bool) -> Result<Vec<String>, String> {
    let list = fs::read_dir(dir).map_err(|e| format!("{}: {}", dir.display(), e.kind()))?;
    let mut names: Vec<String> = list
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| keep(n))
        .collect();
    names.sort();
    Ok(names)
}

/// Dumps the database into `spec.dir` as the backup named for `now_secs`, then removes the
/// older backups beyond `spec.keep`.
///
/// # Errors
/// One line. After any error the older backups are all still there and no temp file is.
pub fn backup(tool: &dyn Tool, spec: &Spec<'_>, now_secs: u64) -> Result<Done, String> {
    prepare_dir(spec.dir)?;
    for stale in names_where(spec.dir, is_temp_name)? {
        fs::remove_file(spec.dir.join(&stale))
            .map_err(|e| format!("cannot remove the leftover {stale}: {}", e.kind()))?;
    }
    let name = stamp(now_secs);
    let file = spec.dir.join(&name);
    if fs::symlink_metadata(&file).is_ok() {
        return Err(format!("{name} already exists; not replacing it"));
    }
    let temp = spec.dir.join(temp_name(&name));
    let made = dump_to(tool, spec, &temp).and_then(|bytes| {
        sync(&temp)?;
        fs::rename(&temp, &file)
            .map_err(|e| format!("cannot give the dump its name {name}: {}", e.kind()))?;
        sync(spec.dir)?;
        Ok(bytes)
    });
    let bytes = match made {
        Ok(bytes) => bytes,
        Err(e) => {
            let _ = fs::remove_file(&temp);
            return Err(e);
        }
    };
    let removed = prune(spec.dir, &name, spec.keep)
        .map_err(|e| format!("{name} is written, but removing the old backups failed: {e}"))?;
    Ok(Done {
        file,
        bytes,
        removed,
    })
}

/// Runs the dump into `temp` and checks it: not empty, closed to others, listed by pg_restore.
fn dump_to(tool: &dyn Tool, spec: &Spec<'_>, temp: &Path) -> Result<u64, String> {
    let mut out = OsString::from("--file=");
    out.push(temp);
    let mut args = words(&["--format=custom", "--no-comments"]);
    args.push(out);
    let mut database = OsString::from("--dbname=");
    database.push(spec.url);
    args.push(database);
    tool.run(spec.pg_dump, &args)?;

    let meta = fs::symlink_metadata(temp)
        .map_err(|e| format!("{} wrote no file: {}", spec.pg_dump, e.kind()))?;
    if !meta.is_file() || meta.len() == 0 {
        return Err(format!("{} wrote an empty file", spec.pg_dump));
    }
    fs::set_permissions(temp, fs::Permissions::from_mode(FILE_MODE))
        .map_err(|e| format!("cannot close the dump to other users: {}", e.kind()))?;
    let mut list = words(&["--list"]);
    list.push(temp.as_os_str().to_owned());
    tool.run(spec.pg_restore, &list)
        .map_err(|e| format!("the dump did not verify: {e}"))?;
    Ok(meta.len())
}

/// Flushes `path` (a file or a directory) to disk.
fn sync(path: &Path) -> Result<(), String> {
    fs::File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("cannot sync {}: {}", path.display(), e.kind()))
}

/// Removes the oldest backups so that `keep` stay, `new` among them (it is never removed).
fn prune(dir: &Path, new: &str, keep: u32) -> Result<Vec<String>, String> {
    let mut older = names_where(dir, is_backup_name)?;
    older.retain(|n| n != new);
    let room = usize::try_from(keep.max(1) - 1).unwrap_or(usize::MAX);
    let excess = older.len().saturating_sub(room);
    let mut removed = Vec::new();
    for name in older.drain(..excess) {
        fs::remove_file(dir.join(&name)).map_err(|e| format!("{name}: {}", e.kind()))?;
        removed.push(name);
    }
    Ok(removed)
}

/// Restores `file` into the database `url` names, which must be empty of kbf's schema: one
/// transaction, stopping at the first error, with the owner and privileges of the dump left
/// out (the role that connects owns what is made).
///
/// # Errors
/// One line; the database is as it was.
pub fn restore(tool: &dyn Tool, pg_restore: &str, url: &str, file: &Path) -> Result<(), String> {
    match fs::symlink_metadata(file) {
        Ok(m) if m.is_file() => {}
        _ => return Err(format!("{}: not a regular file", file.display())),
    }
    let mut args = words(&[
        "--exit-on-error",
        "--single-transaction",
        "--no-owner",
        "--no-privileges",
    ]);
    let mut database = OsString::from("--dbname=");
    database.push(url);
    args.push(database);
    args.push(file.as_os_str().to_owned());
    tool.run(pg_restore, &args)
}

#[cfg(test)]
#[path = "backup_tests.rs"]
mod tests;
