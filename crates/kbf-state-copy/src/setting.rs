//! The one state-store setting a frontend reads: where it is, what it says, and how it is
//! switched to Postgres and back.
//!
//! A frontend takes `--state-store` from `kbf.toml` (the `state-store` key of the
//! `[scheduler]` table, which is what `kbf-installer setup` writes) or from its command line
//! (a `--state-store=URL` in the `ExecStart` of a unit file or drop-in). The tool edits
//! whichever file the operator names and nothing else in it: the new text is the old text
//! with the value replaced, so comments, order and spacing stay.
//!
//! Switching is staged so that the part that can fail is done before the copy: the original
//! is written byte for byte to `<file>.pre-postgres` and the new text to a temporary file
//! beside it, both with the original's mode and owner and synced. Once the copy has passed
//! its checks the only step left is a rename of the temporary file over the original. A
//! copy that fails removes both staged files. Rolling back is a rename of the backup over
//! the file.

use std::fs::{self, File, Metadata, OpenOptions, Permissions};
use std::io::{self, Write};
use std::ops::Range;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, fchown};
use std::path::{Path, PathBuf};

use kbf_state::postgres::PostgresConfig;

use crate::error::Error;
use crate::lock::beside;

/// What is added to a setting file's name for the copy of the original.
pub const BACKUP_SUFFIX: &str = ".pre-postgres";

/// What is added to its name for the new text until it is renamed into place.
const TEMP_SUFFIX: &str = ".postgres-new";

/// The flag whose value is the setting in a unit file.
const UNIT_FLAG: &str = "--state-store=";

const WITH_PASSWORD: &str = "the URL for the setting carries a password, which the frontend \
    refuses: give a URL without one, as postgres:///kbf?host=/run/postgresql, with \
    --frontend-url";

const NEEDS_QUOTING: &str = "the URL for the setting has a space, a quote, a backslash or a \
    character outside ASCII, which a setting file would have to quote";

const NOT_SQLITE: &str = "the setting is not a sqlite:// URL with an absolute path, so this \
    frontend does not run on a state file";

const EXISTS: &str = "exists: left by an earlier cutover, or by one that was interrupted: run \
    rollback, or deal with it as the runbook says under an interrupted cutover";

const COPY_KEPT: &str = " (the setting file is unchanged, but the target already holds the copy: \
    drop its schema and set it up again before another try)";

const NO_BACKUP: &str = "does not exist as a regular file: there is nothing to restore";

const NO_KEY: &str = "has no [scheduler] state-store with a string value: the frontend takes \
    its state store from somewhere else";

const ONE_LINE: &str = "needs exactly one line `state-store = \"URL\"` under [scheduler] to edit";

const NO_FLAG: &str = "needs exactly one unquoted --state-store=URL (the form with a space, or \
    inside quotes, is not edited: write it with an equals sign first, or edit the file by hand)";

const CANNOT_REWRITE: &str = "cannot be rewritten safely: the line that was found is not the \
    setting's. Set [scheduler] state-store by hand";

/// Where the frontend's state-store setting is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    /// The `state-store` key of the `[scheduler]` table of this `kbf.toml`.
    Config(PathBuf),
    /// The `--state-store=` flag in this systemd unit file or drop-in.
    UnitFile(PathBuf),
}

impl Place {
    /// The file the operator named.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Place::Config(path) | Place::UnitFile(path) => path.as_path(),
        }
    }

    /// What the setting is called in a report.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Place::Config(_) => "state-store in [scheduler]",
            Place::UnitFile(_) => "--state-store= flag",
        }
    }

    /// What the operator does once the file is changed, before starting the frontend.
    #[must_use]
    pub fn next_step(&self) -> &'static str {
        match self {
            Place::Config(_) => "start the frontend",
            Place::UnitFile(_) => "run systemctl daemon-reload, then start the frontend",
        }
    }
}

/// A setting file, read: its text, and where in it the value is.
#[derive(Debug)]
pub struct Found {
    place: Place,
    /// The file, with every link resolved: the backup and the new text go beside this.
    pub file: PathBuf,
    /// The value the setting has now.
    pub current: String,
    text: String,
    meta: Metadata,
    span: Range<usize>,
}

/// The files of a switch that are made and not yet in place. Dropped, it removes them.
#[derive(Debug)]
pub struct Staged {
    made: Vec<PathBuf>,
}

/// A backup that is there to be restored.
#[derive(Debug)]
pub struct Restore {
    /// The setting file, with every link resolved.
    pub file: PathBuf,
    /// Its backup, beside it.
    pub backup: PathBuf,
}

fn failed_at(path: &Path, err: &io::Error) -> Error {
    Error::Failed(format!("{}: {err}", path.display()))
}

/// A rename of the new text that failed after the copy was committed.
fn copy_kept(path: &Path, err: &io::Error) -> Error {
    Error::Failed(format!("{}: {err}{COPY_KEPT}", path.display()))
}

fn refused(file: &Path, why: &str) -> Error {
    Error::Refused(format!("{} {why}", file.display()))
}

/// The URL as the setting will hold it, if it is one a frontend takes from a setting file.
///
/// # Errors
/// [`Error::Failed`] when the store does not take the URL; [`Error::Refused`] when it carries
/// a password (the frontend refuses a URL with one, and a setting file is not a place for a
/// secret) or has a character that a setting file would have to quote.
pub fn check_url(url: &str) -> Result<&str, Error> {
    let url = url.trim();
    let config = PostgresConfig::parse(url)?;
    if config.has_password() {
        return Err(Error::Refused(WITH_PASSWORD.to_owned()));
    }
    if !url.chars().all(plain) {
        return Err(Error::Refused(NEEDS_QUOTING.to_owned()));
    }
    Ok(url)
}

/// The printable ASCII characters that need no quoting in a TOML string or a unit file:
/// all but the space, `"`, `'` and `\`.
fn plain(c: char) -> bool {
    matches!(c, '!' | '#'..='&' | '('..='[' | ']'..='~')
}

/// Reads the file `place` names and finds the setting in it.
///
/// # Errors
/// [`Error::Failed`] when the file cannot be read; [`Error::Refused`] when it is not UTF-8,
/// not valid TOML (for a config), or does not hold exactly one place to edit.
pub fn find(place: &Place) -> Result<Found, Error> {
    let path = place.path();
    let file = fs::canonicalize(path).map_err(|e| failed_at(path, &e))?;
    let bytes = fs::read(&file).map_err(|e| failed_at(&file, &e))?;
    let meta = fs::metadata(&file).map_err(|e| failed_at(&file, &e))?;
    let Ok(text) = String::from_utf8(bytes) else {
        return Err(refused(&file, "is not UTF-8 text"));
    };
    let (span, current) = match place {
        Place::Config(_) => in_config(&file, &text)?,
        Place::UnitFile(_) => in_unit(&file, &text)?,
    };
    let place = place.clone();
    Ok(Found {
        place,
        file,
        current,
        text,
        meta,
        span,
    })
}

/// Refuses a setting that does not name the state file being copied.
///
/// # Errors
/// [`Error::Refused`] when the setting is not a `sqlite://` URL with an absolute path, or
/// names a file that is not `db` (its canonical name). The message never quotes a value that
/// is not a `sqlite://` path, since a Postgres URL may hold a password.
pub fn names_file(current: &str, db: &Path) -> Result<(), Error> {
    let path = current.strip_prefix("sqlite://").unwrap_or_default();
    if !path.starts_with('/') {
        return Err(Error::Refused(NOT_SQLITE.to_owned()));
    }
    let named = fs::canonicalize(path).map_err(|e| unresolved(path, &e))?;
    if named == db {
        return Ok(());
    }
    let (named, db) = (named.display(), db.display());
    let text = format!("the setting names the state file {named}, not {db}");
    Err(Error::Refused(text))
}

fn unresolved(path: &str, err: &io::Error) -> Error {
    let text = format!("the setting names the state file {path}, which cannot be read: {err}");
    Error::Refused(text)
}

impl Found {
    /// The file's text with the setting's value replaced by `url` and nothing else changed.
    ///
    /// # Errors
    /// [`Error::Refused`] when the new text of a config does not parse to the old one with
    /// that one value changed.
    pub fn rewritten(&self, url: &str) -> Result<Vec<u8>, Error> {
        let value = match self.place {
            Place::Config(_) => format!("\"{url}\""),
            Place::UnitFile(_) => url.replace('%', "%%").replace('$', "$$"),
        };
        let mut out = String::with_capacity(self.text.len() + value.len());
        out.push_str(&self.text[..self.span.start]);
        out.push_str(&value);
        out.push_str(&self.text[self.span.end..]);
        if let Place::Config(_) = self.place {
            same_but_for_url(&self.file, &self.text, &out, url)?;
        }
        Ok(out.into_bytes())
    }

    /// The backup's name.
    #[must_use]
    pub fn backup(&self) -> PathBuf {
        beside(&self.file, BACKUP_SUFFIX)
    }

    fn temp(&self) -> PathBuf {
        beside(&self.file, TEMP_SUFFIX)
    }

    /// Refuses when what an earlier cutover left is there, finished or interrupted.
    ///
    /// # Errors
    /// [`Error::Refused`] when `<file>.pre-postgres` or `<file>.postgres-new` exists, whatever
    /// it is.
    pub fn require_no_backup(&self) -> Result<(), Error> {
        for path in [self.backup(), self.temp()] {
            if fs::symlink_metadata(&path).is_ok() {
                return Err(refused(&path, EXISTS));
            }
        }
        Ok(())
    }

    /// Writes the backup (this file's text, byte for byte) and `rewritten` beside the file,
    /// each with this file's mode and owner, and syncs them. Nothing is replaced.
    ///
    /// # Errors
    /// [`Error::Failed`] when a file cannot be made, including when either name is taken;
    /// whatever was made is removed.
    pub fn stage(&self, rewritten: &[u8]) -> Result<Staged, Error> {
        let mut staged = Staged { made: Vec::new() };
        staged.add(&self.backup(), self.text.as_bytes(), &self.meta)?;
        staged.add(&self.temp(), rewritten, &self.meta)?;
        Ok(staged)
    }
}

impl Staged {
    fn add(&mut self, path: &Path, bytes: &[u8], like: &Metadata) -> Result<(), Error> {
        let file = create(path).map_err(|e| failed_at(path, &e))?;
        self.made.push(path.to_path_buf());
        fill(file, bytes, like).map_err(|e| failed_at(path, &e))
    }

    /// Renames the new text over the file, which keeps the backup, and syncs the directory.
    ///
    /// # Errors
    /// [`Error::Failed`] when the rename or the sync fails; if the rename fails, the staged
    /// files are removed, and the message says that the copy is in the target all the same.
    pub fn put_in_place(mut self, found: &Found) -> Result<(), Error> {
        let renamed = fs::rename(found.temp(), &found.file);
        renamed.map_err(|e| copy_kept(&found.file, &e))?;
        self.made.clear();
        sync_dir(&found.file)
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        for path in &self.made {
            let _ = fs::remove_file(path);
        }
    }
}

fn create(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    options.open(path)
}

/// Writes `bytes` and gives the file `like`'s mode and owner (the mode is set after the
/// write so that it does not depend on the umask), then syncs it.
fn fill(mut file: File, bytes: &[u8], like: &Metadata) -> io::Result<()> {
    file.write_all(bytes)?;
    file.set_permissions(Permissions::from_mode(like.mode() & 0o7777))?;
    fchown(&file, Some(like.uid()), Some(like.gid()))?;
    file.sync_all()
}

fn sync_dir(file: &Path) -> Result<(), Error> {
    let dir = file.parent().unwrap_or(Path::new("/"));
    let synced = File::open(dir).and_then(|handle| handle.sync_all());
    synced.map_err(|e| unsynced(dir, &e))
}

fn unsynced(dir: &Path, err: &io::Error) -> Error {
    let dir = dir.display();
    Error::Failed(format!("{dir}: {err} (the rename itself was done)"))
}

/// Finds the backup that goes with the file `place` names.
///
/// # Errors
/// [`Error::Failed`] when the file cannot be resolved; [`Error::Refused`] when
/// `<file>.pre-postgres` is not a regular file.
pub fn find_backup(place: &Place) -> Result<Restore, Error> {
    let path = place.path();
    let file = fs::canonicalize(path).map_err(|e| failed_at(path, &e))?;
    let backup = beside(&file, BACKUP_SUFFIX);
    let present = fs::symlink_metadata(&backup).is_ok_and(|m| m.is_file());
    if !present {
        return Err(refused(&backup, NO_BACKUP));
    }
    Ok(Restore { file, backup })
}

impl Restore {
    /// Renames the backup over the file: the file is the backup, byte for byte, with the
    /// backup's mode and owner, and the backup is gone.
    ///
    /// # Errors
    /// [`Error::Failed`] when the rename or the sync of the directory fails.
    pub fn apply(&self) -> Result<(), Error> {
        let renamed = fs::rename(&self.backup, &self.file);
        renamed.map_err(|e| failed_at(&self.file, &e))?;
        sync_dir(&self.file)
    }
}

fn not_toml(file: &Path, err: &toml::de::Error) -> Error {
    refused(file, &format!("is not valid TOML: {}", err.message()))
}

fn parse_config(file: &Path, text: &str) -> Result<toml::Value, Error> {
    let parsed = toml::from_str::<toml::Table>(text);
    let table = parsed.map_err(|e| not_toml(file, &e))?;
    Ok(toml::Value::Table(table))
}

/// The value of the `[scheduler] state-store` key and the span of the quoted value on its
/// line.
fn in_config(file: &Path, text: &str) -> Result<(Range<usize>, String), Error> {
    let parsed = parse_config(file, text)?;
    let key = parsed.get("scheduler").and_then(|s| s.get("state-store"));
    let Some(toml::Value::String(current)) = key else {
        return Err(refused(file, NO_KEY));
    };
    let span = one_span(file, config_spans(text), ONE_LINE)?;
    Ok((span, current.clone()))
}

/// Refuses an edit that changed anything but the one value: the new text must parse to what
/// the old text did with the `[scheduler] state-store` value set to `url`.
fn same_but_for_url(file: &Path, before: &str, after: &str, url: &str) -> Result<(), Error> {
    let mut want = parse_config(file, before)?;
    let scheduler = want.get_mut("scheduler");
    let slot = scheduler.and_then(|s| s.get_mut("state-store"));
    let value = toml::Value::String(url.to_owned());
    *slot.expect("the key was found when the file was read") = value;
    if parse_config(file, after)? == want {
        return Ok(());
    }
    Err(refused(file, CANNOT_REWRITE))
}

/// The span of the quoted value of every line `state-store = "..."` (or with single quotes)
/// in a `[scheduler]` table, found by reading lines. The caller checks the result against a
/// real parse, so a line that merely looks like one is caught there.
fn config_spans(text: &str) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    let mut inside = false;
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        let body = line.trim_start();
        if body.starts_with('[') {
            inside = body.strip_prefix("[scheduler]").is_some_and(only_comment);
        } else if inside {
            spans.extend(key_span(line, start));
        }
        start += line.len();
    }
    spans
}

/// Whether nothing but a comment is left of a line.
fn only_comment(rest: &str) -> bool {
    matches!(rest.trim_start().chars().next(), None | Some('#'))
}

/// The span of the quoted value if `line` (which starts at `start` in the text) is
/// `state-store = "value"` and nothing but a comment follows.
fn key_span(line: &str, start: usize) -> Option<Range<usize>> {
    let rest = line.trim_start().strip_prefix("state-store")?;
    let value = rest.trim_start().strip_prefix('=')?.trim_start();
    let quote = value.chars().next().filter(|&c| matches!(c, '"' | '\''))?;
    let end = 1 + value[1..].find(quote)?;
    only_comment(&value[end + 1..]).then_some(())?;
    let at = start + line.len() - value.len();
    Some(at..at + end + 1)
}

/// The value of the one `--state-store=` flag, with the unit file's escapes undone, and its
/// span.
fn in_unit(file: &Path, text: &str) -> Result<(Range<usize>, String), Error> {
    let span = one_span(file, unit_spans(text), NO_FLAG)?;
    let current = text[span.clone()].replace("%%", "%").replace("$$", "$");
    Ok((span, current))
}

/// The span of the value of every `--state-store=` in a line that is not a comment: from the
/// equals sign to the next white space.
fn unit_spans(text: &str) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        if !line.trim_start().starts_with(['#', ';']) {
            spans.extend(flag_spans(line, start));
        }
        start += line.len();
    }
    spans
}

fn flag_spans(line: &str, start: usize) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    for (at, _) in line.match_indices(UNIT_FLAG) {
        let before = line[..at].chars().next_back();
        if before.is_none_or(char::is_whitespace) {
            let from = at + UNIT_FLAG.len();
            let gap = line[from..].find(char::is_whitespace);
            let to = gap.map_or(line.len(), |n| from + n);
            spans.push(start + from..start + to);
        }
    }
    spans
}

fn one_span(file: &Path, mut spans: Vec<Range<usize>>, need: &str) -> Result<Range<usize>, Error> {
    let found = spans.len();
    match (spans.pop(), found) {
        (Some(span), 1) => Ok(span),
        _ => Err(refused(file, &format!("{need}, and found {found}"))),
    }
}
