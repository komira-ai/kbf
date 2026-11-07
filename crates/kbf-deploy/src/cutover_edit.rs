//! The edit of `kbf.toml` that `job-service apply` makes, and the restore of `back-out`.
//!
//! The edit is made on the text, line by line, so that every comment, every other table and
//! the order of the other keys stay as they were: each line of the `[executor]` table that
//! sets one of [`KEYS`] is taken out, and the switch's lines are put right after the table's
//! header (a file with no `[executor]` table gets one at its end). The result is then parsed
//! and must be the old file with exactly those keys changed; a file the line edit cannot
//! rewrite so (a key written as a dotted or quoted name, a value over several lines, a
//! header written `[ executor ]`) is refused, and nothing is written.
//!
//! The original is kept byte for byte beside the file as `<file>.pre-job-service`, with the
//! file's mode and owner, and the new text is staged as `<file>.job-service-new`, also with
//! them, so that `kbf --check` can read it before it is put in place by a rename. Both are
//! made with `create_new`: a backup that is there already (from a switch that was not backed
//! out) is refused and left alone. `back-out` renames the backup over the file.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use crate::cutover::{CutoverError, Executor, KEYS, Settings, TABLE};

/// The suffix of the byte-exact copy of the file from before the switch.
pub(crate) const BACKUP_SUFFIX: &str = ".pre-job-service";

/// The suffix of the new text while it is checked, before it is renamed over the file.
pub(crate) const STAGED_SUFFIX: &str = ".job-service-new";

/// `path` with `suffix` added to its file name.
pub(crate) fn beside(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// The text of a line without its comment and surrounding space: a header or `key = value`
/// is matched on this. A `#` inside a quoted value is not cut, because the key is all that
/// is read from it.
fn bare(line: &str) -> &str {
    line.split('#').next().unwrap_or("").trim()
}

/// Whether `line` is a table header (`[name]` or `[[name]]`).
fn header(line: &str) -> bool {
    bare(line).starts_with('[')
}

/// The key a `key = value` line sets, as written before the `=`.
fn key_of(line: &str) -> Option<&str> {
    line.split_once('=').map(|(key, _)| key.trim())
}

/// The lines that set the switch's keys, as TOML writes them.
fn lines_of(settings: &Settings) -> Vec<String> {
    settings
        .0
        .iter()
        .filter_map(|(k, v)| v.toml().map(|v| format!("{k} = {v}")))
        .collect()
}

/// The old table with the switch's settings made: what the new file must parse to.
fn expected(old: &toml::Table, settings: &Settings) -> toml::Table {
    let mut doc = old.clone();
    let mut table = match doc.remove(TABLE) {
        Some(toml::Value::Table(t)) => t,
        _ => toml::Table::new(),
    };
    for (key, value) in &settings.0 {
        match value.toml() {
            Some(v) => {
                table.insert((*key).to_owned(), v);
            }
            None => {
                table.remove(*key);
            }
        }
    }
    doc.insert(TABLE.to_owned(), toml::Value::Table(table));
    doc
}

/// The file `path`, whose text is `text`, with the switch's settings made, or a refusal.
pub(crate) fn rewritten(
    path: &Path,
    text: &str,
    settings: &Settings,
) -> Result<String, CutoverError> {
    let refuse = |why: &str| {
        CutoverError::Refused(format!(
            "{}: cannot rewrite [{TABLE}] line by line: {why}; make the change by hand",
            path.display()
        ))
    };
    let old: toml::Table = text.parse().map_err(|e: toml::de::Error| {
        CutoverError::Refused(format!("{} is not TOML: {}", path.display(), e.message()))
    })?;
    // Checks the `executor` key is a table, with the same message as the plan.
    Executor::parse(path, text)?;
    let lines: Vec<&str> = text.lines().collect();
    // TOML refuses a table defined twice, so a file that parsed has at most one header.
    let head = lines.iter().position(|l| bare(l) == format!("[{TABLE}]"));
    let new_lines = lines_of(settings);
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + new_lines.len() + 2);
    match head {
        None => {
            out.extend(lines.iter().map(|l| (*l).to_owned()));
            if out.last().is_some_and(|l| !l.trim().is_empty()) {
                out.push(String::new());
            }
            out.push(format!("[{TABLE}]"));
            out.extend(new_lines);
        }
        Some(head) => {
            let mut in_table = false;
            for (i, line) in lines.iter().enumerate() {
                if header(line) {
                    in_table = i == head;
                }
                let drop = in_table && i != head && key_of(line).is_some_and(|k| KEYS.contains(&k));
                if !drop {
                    out.push((*line).to_owned());
                }
                if i == head {
                    out.extend(new_lines.iter().cloned());
                }
            }
        }
    }
    let mut new = out.join("\n");
    new.push('\n');
    let parsed: toml::Table = new.parse().map_err(|_| refuse("the result is not TOML"))?;
    if parsed != expected(&old, settings) {
        return Err(refuse(
            "the result is not the file with only the switch's keys changed",
        ));
    }
    Ok(new)
}

/// A file made with `create_new`, given `mode` (after the write, so the umask does not
/// matter) and the owner `uid:gid`, and synced.
fn create(path: &Path, bytes: &[u8], mode: u32, uid: u32, gid: u32) -> std::io::Result<()> {
    let mut f: File = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    let filled = (|| {
        f.write_all(bytes)?;
        f.set_permissions(fs::Permissions::from_mode(mode & 0o7777))?;
        std::os::unix::fs::fchown(&f, Some(uid), Some(gid))?;
        f.sync_all()
    })();
    if filled.is_err() {
        // The file is this call's own (`create_new` made it): a half-written one goes.
        let _ = fs::remove_file(path);
    }
    filled
}

/// Rename `from` over `to` and sync their directory, so that the rename survives a power cut.
fn rename_synced(from: &Path, to: &Path) -> Result<(), CutoverError> {
    fs::rename(from, to)
        .and_then(|()| File::open(to.parent().unwrap_or(Path::new("/")))?.sync_all())
        .map_err(|e| {
            CutoverError::Failed(format!(
                "cannot rename {} over {}: {e}",
                from.display(),
                to.display()
            ))
        })
}

/// The new text, staged beside the file with the backup of the old.
#[derive(Debug)]
pub(crate) struct Staged {
    /// The file as given, canonicalised: the backup goes beside its real place.
    pub(crate) file: PathBuf,
    pub(crate) backup: PathBuf,
    pub(crate) staged: PathBuf,
    /// The files this value made and has not put in place: dropping the value removes
    /// them, so a switch that stopped before its rename leaves nothing of its own behind.
    made: Vec<PathBuf>,
}

/// The canonical form of the file `config`.
pub(crate) fn canonical(config: &Path) -> Result<PathBuf, CutoverError> {
    fs::canonicalize(config)
        .map_err(|e| CutoverError::Refused(format!("--config {}: {e}", config.display())))
}

/// Refuse when the backup or the staged file of `file` is there already.
pub(crate) fn require_no_backup(file: &Path) -> Result<(), CutoverError> {
    for suffix in [BACKUP_SUFFIX, STAGED_SUFFIX] {
        let path = beside(file, suffix);
        if fs::symlink_metadata(&path).is_ok() {
            return Err(CutoverError::Refused(format!(
                "{} is there already: a switch was made and not backed out, or one stopped \
                 half way; back it out (job-service back-out) or move the file away by hand",
                path.display()
            )));
        }
    }
    Ok(())
}

impl Staged {
    /// Write the backup of `file` (whose text is `old`) and the staged `new` beside it.
    pub(crate) fn write(file: &Path, old: &str, new: &str) -> Result<Staged, CutoverError> {
        let failed = |what: &Path, e: std::io::Error| {
            CutoverError::Failed(format!("cannot write {}: {e}", what.display()))
        };
        require_no_backup(file)?;
        let meta = fs::metadata(file)
            .map_err(|e| CutoverError::Refused(format!("--config {}: {e}", file.display())))?;
        let mut staged = Staged {
            file: file.to_path_buf(),
            backup: beside(file, BACKUP_SUFFIX),
            staged: beside(file, STAGED_SUFFIX),
            made: Vec::new(),
        };
        let (mode, uid, gid) = (meta.mode(), meta.uid(), meta.gid());
        for (path, bytes) in [(&staged.backup, old), (&staged.staged, new)] {
            let path = path.clone();
            // A file that was there is not this value's to remove: `create_new` failed.
            create(&path, bytes.as_bytes(), mode, uid, gid).map_err(|e| failed(&path, e))?;
            staged.made.push(path);
        }
        Ok(staged)
    }

    /// Rename the staged file over the file. The backup stays: it is the back-out.
    pub(crate) fn put_in_place(mut self) -> Result<(), CutoverError> {
        let done = rename_synced(&self.staged, &self.file);
        if done.is_ok() {
            self.made.clear();
        }
        done
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        for path in &self.made {
            let _ = fs::remove_file(path);
        }
    }
}

/// The backup of `file`, as text, or a refusal when there is none.
pub(crate) fn read_backup(file: &Path) -> Result<(PathBuf, String), CutoverError> {
    let backup = beside(file, BACKUP_SUFFIX);
    match fs::symlink_metadata(&backup) {
        Ok(meta) if meta.is_file() => {}
        _ => {
            return Err(CutoverError::Refused(format!(
                "{} is not there (or is not a regular file): there is no switch to back out",
                backup.display()
            )));
        }
    }
    let text = fs::read_to_string(&backup)
        .map_err(|e| CutoverError::Refused(format!("{}: {e}", backup.display())))?;
    Ok((backup, text))
}

/// Rename the backup of `file` over it: the file is the original again, byte for byte,
/// with its mode and owner, and the backup is gone.
pub(crate) fn restore(file: &Path, backup: &Path) -> Result<(), CutoverError> {
    rename_synced(backup, file)
}

#[cfg(test)]
#[path = "cutover_edit_tests.rs"]
mod tests;
