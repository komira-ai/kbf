//! The node's metadata log, saved before a release first starts on it and put back when the
//! host goes back to the release that ran before.
//!
//! A new release may raise the log's machine version the first time it starts (kbf#1278), and
//! an older binary refuses a raised log. So a rollback that only moves the links would leave a
//! node that cannot start. The installer therefore keeps a copy of the log as the older
//! release left it, and puts it back before the older release starts again.
//!
//! Where the copy is made: at the first start of a release on trial, from `boot-check`, which
//! the unit runs as `ExecStartPre=` after the old process has stopped and before the new one
//! starts. That is the one point at which nothing writes the log; the helper's restart is a
//! single `systemctl restart` and has no stop of its own to hook. (macOS has no start hook, so
//! it keeps no copy.)
//!
//! The copy lives in the directory of the release the log belongs to, the one that was
//! running, and goes with that release when `gc` collects it:
//!
//! ```text
//! releases/<older>/meta-log.backup/
//!   for     the release that was started on the log, and one line
//!   log/    the log's directory as it was: files, directories, modes and owners (absent when
//!           the node had no log yet)
//! ```
//!
//! It is built under a temporary name, synced, and renamed into place, so it is whole or not
//! there. One copy is kept: making one removes the others.
//!
//! The copy is put back at a start of the release that holds it as `current`, which is the
//! case exactly when the swap away from it was reverted or rolled back. The log is put back
//! only if it differs from the copy (a release that never started leaves it as it was), and
//! the copy is removed after, so a repeat finds nothing to do. What the log committed while
//! the newer release ran is not kept.

use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, PermissionsExt, chown};
use std::path::{Path, PathBuf};

use kbf_host::HostPath;

use crate::CommitId;
use crate::helper::host_mode;
use crate::layout::Layout;

/// The copy's directory, inside the release it belongs to.
pub const BACKUP: &str = "meta-log.backup";
/// Where a copy is built before it is renamed to [`BACKUP`].
const BACKUP_TMP: &str = ".meta-log.backup.tmp";
/// The file in a copy that names the release started on the log.
const FOR: &str = "for";
/// The copy of the log's directory.
const LOG: &str = "log";
/// The file in the state directory that says why an activation was refused for want of a
/// copy; the helper reports it in `status.json` and removes it.
pub const REFUSED: &str = "meta-log.refused";
/// The most of the refusal file that is read.
const REFUSED_MAX: u64 = 4096;

/// Why the log could not be saved or put back.
#[derive(Debug, thiserror::Error)]
pub enum MetaLogError {
    #[error("the metadata directory: {0}")]
    Dir(String),
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("{0} is neither a file nor a directory, and is not copied")]
    Kind(PathBuf),
    #[error("{0} does not name a release")]
    For(PathBuf),
}

fn io_at(path: &Path) -> impl FnOnce(io::Error) -> MetaLogError + '_ {
    move |source| MetaLogError::Io {
        path: path.to_owned(),
        source,
    }
}

/// What [`save`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Saved {
    /// This host's mode keeps no metadata log.
    NoLog,
    /// A copy for this transition was made.
    Taken,
    /// There already was one for this transition; it is as it was.
    Kept,
}

/// What [`restore_owed`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Restored {
    /// The release holds no copy.
    Nothing,
    /// The log was as the copy has it; the copy was removed.
    Unchanged,
    /// The log was put back as the copy has it (after `from` had started on it); the copy
    /// was removed.
    Put { from: CommitId },
}

/// The metadata log's directory, as `kbf.toml` names it, under the host root; `None` for a mode
/// that keeps none (or no `kbf.toml`). An `all-in-one` host names it in `[stores] meta-dir`; a
/// storage node in `[storage] meta_dir`, which defaults to `meta` in `[dirs] state` (default
/// `/var/lib/kbf`), as the node's enrollment reads it.
pub fn dir(host: &Layout) -> Result<Option<PathBuf>, MetaLogError> {
    let Some(table) = host_mode::read_table(host).map_err(MetaLogError::Dir)? else {
        return Ok(None);
    };
    let get = |t: &str, k: &str| {
        table
            .get(t)
            .and_then(toml::Value::as_table)
            .and_then(|t| t.get(k))
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
    };
    let shown = host.kbf_toml();
    let named = match table.get("mode").and_then(toml::Value::as_str) {
        Some("all-in-one") => get("stores", "meta-dir").ok_or_else(|| {
            MetaLogError::Dir(format!(
                "{shown} is an all-in-one host and has no [stores] meta-dir"
            ))
        })?,
        Some("storage" | "storage-and-daemon") => get("storage", "meta_dir").unwrap_or_else(|| {
            let state = get("dirs", "state").unwrap_or_else(|| "/var/lib/kbf".to_owned());
            format!("{}/meta", state.trim_end_matches('/'))
        }),
        _ => return Ok(None),
    };
    let path = HostPath::new(&named)
        .map_err(|e| MetaLogError::Dir(format!("{shown} names {named:?}: {e}")))?;
    Ok(Some(host.join(&path)))
}

pub(crate) fn backup_of(host: &Layout, holder: &CommitId) -> PathBuf {
    host.join(&host.release_dir(holder)).join(BACKUP)
}

/// The release a copy was made for, `None` when there is no copy.
fn read_for(backup: &Path) -> Result<Option<CommitId>, MetaLogError> {
    let file = backup.join(FOR);
    match fs::read_to_string(&file) {
        Ok(text) => CommitId::new(text.trim_end())
            .map(Some)
            .map_err(|_| MetaLogError::For(file)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_at(&file)(e)),
    }
}

/// Make the copy of the log that `next` is about to start on, in the directory of `holder`, the
/// release that ran on it. A copy for this transition that is there is kept as it is: the log
/// may have changed since (the release started), and the copy is the log before it did. A copy
/// for another release is put back first (it is still owed), and then replaced. Every other
/// release's copy is removed.
pub fn save(host: &Layout, holder: &CommitId, next: &CommitId) -> Result<Saved, MetaLogError> {
    let Some(meta) = dir(host)? else {
        return Ok(Saved::NoLog);
    };
    let backup = backup_of(host, holder);
    match read_for(&backup)? {
        Some(made_for) if made_for == *next => return Ok(Saved::Kept),
        Some(_) => {
            put_back(&meta, &backup)?;
        }
        None => {}
    }
    let tmp = backup.with_file_name(BACKUP_TMP);
    remove_if_there(&tmp)?;
    if let Err(e) = build(&meta, &tmp, next) {
        let _ = remove_if_there(&tmp);
        return Err(e);
    }
    remove_if_there(&backup)?;
    fs::rename(&tmp, &backup).map_err(io_at(&backup))?;
    sync_dir(backup.parent().expect("a copy is in a release"))?;
    prune(host, holder)?;
    Ok(Saved::Taken)
}

/// Put the log back if `current` holds a copy of it: the swap away from `current` was reverted
/// or rolled back. Run before every start, with the old process gone.
pub fn restore_owed(host: &Layout, current: &CommitId) -> Result<Restored, MetaLogError> {
    let backup = backup_of(host, current);
    let Some(from) = read_for(&backup)? else {
        return Ok(Restored::Nothing);
    };
    let Some(meta) = dir(host)? else {
        return Err(MetaLogError::Dir(format!(
            "{} holds a copy of the metadata log and {} names none",
            host.release_dir(current),
            host.kbf_toml()
        )));
    };
    Ok(if put_back(&meta, &backup)? {
        Restored::Put { from }
    } else {
        Restored::Unchanged
    })
}

/// The copy at `tmp`: `for`, and the log's directory when there is one.
fn build(meta: &Path, tmp: &Path, next: &CommitId) -> Result<(), MetaLogError> {
    fs::create_dir(tmp).map_err(io_at(tmp))?;
    let file = tmp.join(FOR);
    let mut out = File::create(&file).map_err(io_at(&file))?;
    io::Write::write_all(&mut out, format!("{next}\n").as_bytes()).map_err(io_at(&file))?;
    out.sync_all().map_err(io_at(&file))?;
    match fs::symlink_metadata(meta) {
        Ok(m) if m.is_dir() => copy_tree(meta, &tmp.join(LOG))?,
        Ok(_) => return Err(MetaLogError::Kind(meta.to_owned())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_at(meta)(e)),
    }
    sync_dir(tmp)
}

/// Make the log the copy at `backup` has, unless it is that already, and remove the copy.
/// Whether the log was changed. The copy goes last, so a kill in between leaves it to be put
/// back again.
fn put_back(meta: &Path, backup: &Path) -> Result<bool, MetaLogError> {
    let log = backup.join(LOG);
    let changed = !same(meta, &log).map_err(io_at(meta))?;
    if changed {
        let wanted = match fs::symlink_metadata(&log) {
            Ok(m) => Some(m),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(io_at(&log)(e)),
        };
        match fs::symlink_metadata(meta) {
            Ok(m) if m.is_dir() => empty(meta)?,
            Ok(_) => return Err(MetaLogError::Kind(meta.to_owned())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if wanted.is_some() {
                    fs::create_dir_all(meta).map_err(io_at(meta))?;
                }
            }
            Err(e) => return Err(io_at(meta)(e)),
        }
        if let Some(wanted) = wanted {
            copy_children(&log, meta)?;
            attrs(meta, &wanted)?;
            sync_dir(meta)?;
        }
    }
    fs::remove_dir_all(backup).map_err(io_at(backup))?;
    sync_dir(backup.parent().expect("a copy is in a release"))?;
    Ok(changed)
}

/// Remove the copy of every release but `keep`. A link in `releases/` is not followed.
fn prune(host: &Layout, keep: &CommitId) -> Result<(), MetaLogError> {
    let releases = host.join(&host.releases());
    for item in fs::read_dir(&releases).map_err(io_at(&releases))? {
        let item = item.map_err(io_at(&releases))?;
        let is_dir = item.file_type().map_err(io_at(&item.path()))?.is_dir();
        if !is_dir || item.file_name() == keep.as_str() {
            continue;
        }
        for name in [BACKUP, BACKUP_TMP] {
            remove_if_there(&item.path().join(name))?;
        }
    }
    Ok(())
}

fn remove_if_there(path: &Path) -> Result<(), MetaLogError> {
    match fs::remove_dir_all(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other.map_err(io_at(path)),
    }
}

/// Copy `src`, a directory, to `dst`, which is made; files and directories only, with their
/// modes and owners, each synced.
fn copy_tree(src: &Path, dst: &Path) -> Result<(), MetaLogError> {
    let meta = fs::symlink_metadata(src).map_err(io_at(src))?;
    fs::create_dir(dst).map_err(io_at(dst))?;
    copy_children(src, dst)?;
    attrs(dst, &meta)?;
    sync_dir(dst)
}

/// Copy what is in the directory `src` into the directory `dst`.
fn copy_children(src: &Path, dst: &Path) -> Result<(), MetaLogError> {
    for item in fs::read_dir(src).map_err(io_at(src))? {
        let item = item.map_err(io_at(src))?;
        let (from, to) = (item.path(), dst.join(item.file_name()));
        let kind = item.file_type().map_err(io_at(&from))?;
        if kind.is_dir() {
            copy_tree(&from, &to)?;
        } else if kind.is_file() {
            copy_file(&from, &to)?;
        } else {
            return Err(MetaLogError::Kind(from));
        }
    }
    Ok(())
}

fn copy_file(from: &Path, to: &Path) -> Result<(), MetaLogError> {
    let meta = fs::symlink_metadata(from).map_err(io_at(from))?;
    let mut input = File::open(from).map_err(io_at(from))?;
    let mut out = File::create_new(to).map_err(io_at(to))?;
    io::copy(&mut input, &mut out).map_err(io_at(to))?;
    attrs(to, &meta)?;
    out.sync_all().map_err(io_at(to))
}

/// The owner and mode of `meta` on `path`.
fn attrs(path: &Path, meta: &fs::Metadata) -> Result<(), MetaLogError> {
    chown(path, Some(meta.uid()), Some(meta.gid())).map_err(io_at(path))?;
    fs::set_permissions(path, fs::Permissions::from_mode(meta.mode() & 0o7777)).map_err(io_at(path))
}

fn sync_dir(dir: &Path) -> Result<(), MetaLogError> {
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(io_at(dir))
}

/// Remove everything in the directory `dir`, a link by itself.
fn empty(dir: &Path) -> Result<(), MetaLogError> {
    for item in fs::read_dir(dir).map_err(io_at(dir))? {
        let item = item.map_err(io_at(dir))?;
        let path = item.path();
        if item.file_type().map_err(io_at(&path))?.is_dir() {
            fs::remove_dir_all(&path).map_err(io_at(&path))?;
        } else {
            fs::remove_file(&path).map_err(io_at(&path))?;
        }
    }
    sync_dir(dir)
}

/// Whether the directories `a` and `b` hold the same names, kinds and file bytes; a directory
/// that is not there holds nothing.
fn same(a: &Path, b: &Path) -> io::Result<bool> {
    let (left, right) = (names(a)?, names(b)?);
    if left != right {
        return Ok(false);
    }
    for name in left {
        let (x, y) = (a.join(&name), b.join(&name));
        let (mx, my) = (fs::symlink_metadata(&x)?, fs::symlink_metadata(&y)?);
        let alike = match (mx.is_dir(), my.is_dir()) {
            (true, true) => same(&x, &y)?,
            (false, false) => {
                mx.is_file() && my.is_file() && mx.len() == my.len() && same_bytes(&x, &y)?
            }
            _ => false,
        };
        if !alike {
            return Ok(false);
        }
    }
    Ok(true)
}

fn names(dir: &Path) -> io::Result<Vec<std::ffi::OsString>> {
    let items = match fs::read_dir(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        listed => listed?,
    };
    let mut names = items
        .map(|i| i.map(|i| i.file_name()))
        .collect::<io::Result<Vec<_>>>()?;
    names.sort();
    Ok(names)
}

fn same_bytes(a: &Path, b: &Path) -> io::Result<bool> {
    let (mut x, mut y) = (File::open(a)?, File::open(b)?);
    let (mut bx, mut by) = (vec![0u8; 1 << 16], vec![0u8; 1 << 16]);
    loop {
        let n = read_full(&mut x, &mut bx)?;
        if n != read_full(&mut y, &mut by)? || bx[..n] != by[..n] {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
    }
}

/// Fill `buf` from `r`, or as much as there is.
fn read_full(r: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut at = 0;
    while at < buf.len() {
        match r.read(&mut buf[at..])? {
            0 => break,
            n => at += n,
        }
    }
    Ok(at)
}

/// Say why an activation was refused for want of a copy, for the helper's `status.json`.
pub fn note_refusal(host: &Layout, why: &str) -> io::Result<()> {
    let dir = host.join(&host.state_dir());
    let (tmp, file) = (dir.join(format!("{REFUSED}.tmp")), dir.join(REFUSED));
    fs::write(&tmp, why)?;
    File::open(&tmp)?.sync_all()?;
    fs::rename(&tmp, &file)
}

/// The refusal [`note_refusal`] left; `None` when there is none.
pub fn refusal(host: &Layout) -> Option<String> {
    let mut bytes = Vec::new();
    File::open(host.join(&host.state_dir()).join(REFUSED))
        .and_then(|f| f.take(REFUSED_MAX).read_to_end(&mut bytes))
        .ok()?;
    Some(String::from_utf8_lossy(&bytes).trim_end().to_owned())
}

/// Remove the refusal once it is reported.
pub fn clear_refusal(host: &Layout) {
    let _ = fs::remove_file(host.join(&host.state_dir()).join(REFUSED));
}

#[cfg(test)]
#[path = "meta_log_tests.rs"]
mod tests;
