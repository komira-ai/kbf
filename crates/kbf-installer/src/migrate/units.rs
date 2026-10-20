//! The units a host runs kbf under: their names, and the one edit the migration makes to
//! them, pointing the old release link at the new one.
//!
//! The old layout's units ran `/usr/local/kbf/current/kbf ...` (the deploy kit rendered
//! `KBF_BIN_DIR` as that link, docs/design/pull-updates.md section 3); the new tree keeps
//! its link under the installer's tree ([`crate::layout::Layout::current`]). A systemd
//! unit is changed only in the value of an `Exec*=` or `Environment=` line, a launchd
//! plist only in a `<string>` line, and only where the old path is a whole path (what
//! precedes it opens a value, what follows it ends the component); every other byte stays.
//! The first time a unit is changed its original is kept in the migration's backup
//! directory, byte for byte, which is what a rollback puts back. The file is opened
//! without following a link and must be root's.

use std::os::fd::OwnedFd;

use super::tree::{self, Rules, TreeError};

/// The link the old layout's units ran from.
pub const OLD_BIN: &str = "/usr/local/kbf/current";
/// The biggest unit file read.
const UNIT_CAP: u64 = 64 * 1024;

/// The settings whose values name a program or the binary directory.
const KEYS: [&str; 8] = [
    "ExecStart=",
    "ExecStartPre=",
    "ExecStartPost=",
    "ExecCondition=",
    "ExecReload=",
    "ExecStop=",
    "ExecStopPost=",
    "Environment=",
];

/// How a unit is written and run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A systemd service: `<name>.service`.
    Systemd,
    /// A launchd job: `<label>.plist`.
    Launchd,
}

/// A unit, by the name of its file: lowercase letters, digits and `-`, then `.service`;
/// or, for launchd, lowercase letters, digits, `-` and `.`, then `.plist`. Instances and
/// templates (`@`) are not named here; the frontend's instances are the new layout's and
/// never an old unit.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct Unit(String);

/// Why a string is not a [`Unit`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "{0:?} is not a unit name (lowercase letters, digits and '-', then .service; or a launchd label then .plist)"
)]
pub struct NotAUnit(pub String);

impl Unit {
    pub fn new(name: &str) -> Result<Unit, NotAUnit> {
        let (stem, dots) = match (name.strip_suffix(".service"), name.strip_suffix(".plist")) {
            (Some(stem), _) => (stem, false),
            (None, Some(stem)) => (stem, true),
            (None, None) => ("", false),
        };
        let plain = |b: u8| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-') || (dots && b == b'.');
        let edge = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric());
        if edge(stem.chars().next()) && edge(stem.chars().last()) && stem.bytes().all(plain) {
            Ok(Unit(name.to_owned()))
        } else {
            Err(NotAUnit(name.to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn kind(&self) -> Kind {
        if self.0.ends_with(".plist") {
            Kind::Launchd
        } else {
            Kind::Systemd
        }
    }

    /// The name without its suffix: the launchd label, or the service's stem.
    pub fn stem(&self) -> &str {
        self.0.rsplit_once('.').map_or(&self.0, |(stem, _)| stem)
    }
}

impl TryFrom<String> for Unit {
    type Error = NotAUnit;

    fn try_from(name: String) -> Result<Unit, NotAUnit> {
        Unit::new(&name)
    }
}

impl From<Unit> for String {
    fn from(unit: Unit) -> String {
        unit.0
    }
}

impl std::fmt::Display for Unit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What the migration did to one unit file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnitOutcome {
    /// Its old path was replaced (the original is kept).
    Rewritten,
    /// It runs from the new link already.
    Already,
    /// It names neither link: it does not run kbf from a release, and is left alone.
    NotOurs,
    /// There is no file of that name.
    Missing,
}

/// Whether `line` of a unit of `kind` is one whose value may name the binary directory.
fn holds_path(kind: Kind, line: &str) -> bool {
    let line = line.trim_start();
    match kind {
        Kind::Systemd => KEYS.iter().any(|k| line.starts_with(k)),
        Kind::Launchd => line.starts_with("<string>"),
    }
}

/// `text` with the old path replaced by `new_bin`, where a line that may hold a path
/// holds it whole: opened by `=`, `:`, a quote, `>` or white space, and closed by `/`, a
/// quote, `<` or white space (or the line's end). `None` when nothing changes.
pub fn rewrite_text(kind: Kind, text: &str, new_bin: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        if !holds_path(kind, line) {
            out.push_str(line);
            continue;
        }
        let mut rest = line;
        let mut before = None::<char>;
        while let Some(at) = rest.find(OLD_BIN) {
            let (head, after) = rest.split_at(at + OLD_BIN.len());
            let opens = head[..at].chars().next_back().or(before);
            let opened = opens
                .is_some_and(|c| matches!(c, '=' | ':' | '"' | '\'' | '>') || c.is_whitespace());
            let closed = after
                .chars()
                .next()
                .is_none_or(|c| matches!(c, '/' | '"' | '\'' | '<') || c.is_whitespace());
            out.push_str(&head[..at]);
            out.push_str(if opened && closed { new_bin } else { OLD_BIN });
            before = OLD_BIN.chars().next_back();
            rest = after;
        }
        out.push_str(rest);
    }
    (out != text).then_some(out)
}

/// The flag whose value names the state store.
const STORE_FLAG: &str = "--state-store=";

/// `line` with the value of every `--state-store=sqlite://...` replaced by `url`. The flag
/// must open a word (after white space, a quote or `>`), and its value runs to white space,
/// a quote, `<` or the line's continuation backslash.
fn switch_store_line(line: &str, url: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find(STORE_FLAG) {
        let (head, tail) = rest.split_at(at + STORE_FLAG.len());
        let opened = head[..at]
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '>'));
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '\\'))
            .unwrap_or(tail.len());
        let (value, after) = tail.split_at(end);
        out.push_str(head);
        out.push_str(if opened && value.starts_with("sqlite://") {
            url
        } else {
            value
        });
        rest = after;
    }
    out.push_str(rest);
    out
}

/// `text` with the SQLite store of the command line replaced by `url`: on the lines of an
/// `Exec*=` setting and the lines its trailing backslashes continue (the flags of a unit the
/// kit renders are on those), or a launchd `<string>`. `None` when nothing changes.
pub fn switch_store_text(kind: Kind, text: &str, url: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut continued = false;
    for line in text.split_inclusive('\n') {
        let on_command = continued || holds_path(kind, line);
        continued = on_command && line.trim_end().ends_with('\\');
        if on_command {
            out.push_str(&switch_store_line(line, url));
        } else {
            out.push_str(line);
        }
    }
    (out != text).then_some(out)
}

/// Give the unit file `unit` of `units` the state store `url` in place of its SQLite file,
/// keeping the original in `backup` the first time. `Already` when the unit names a Postgres
/// store; `NotOurs` when it names no store this changes.
pub fn switch_store(
    rules: &Rules,
    units: &OwnedFd,
    backup: &OwnedFd,
    unit: &Unit,
    url: &str,
) -> Result<UnitOutcome, TreeError> {
    let name = unit.as_str();
    let Some(bytes) = tree::read_file(rules, units, name, UNIT_CAP)? else {
        return Ok(UnitOutcome::Missing);
    };
    let text = String::from_utf8_lossy(&bytes);
    let Some(new) = switch_store_text(unit.kind(), &text, url) else {
        let postgres = text.contains("--state-store=postgres");
        return Ok(if postgres {
            UnitOutcome::Already
        } else {
            UnitOutcome::NotOurs
        });
    };
    if tree::read_file(rules, backup, name, UNIT_CAP)?.is_none() {
        tree::replace_file(backup, name, 0o600, &bytes)?;
    }
    tree::replace_file(units, name, 0o644, new.as_bytes())?;
    Ok(UnitOutcome::Rewritten)
}

/// What is to be done to a unit file, from everything that is read to decide it.
enum Judged {
    Nothing(UnitOutcome),
    /// The file changes: `original` is what it holds, `new` what it is to hold, and `kept`
    /// whether `backup` has an original already (the first one is the one kept).
    Change {
        original: Vec<u8>,
        new: String,
        kept: bool,
    },
}

/// Read the unit file `unit` of `units` (the unit directory, open), and the original kept
/// for it in `backup`, judged by the rules, and say what is to be done. Writes nothing.
fn judge(
    rules: &Rules,
    units: &OwnedFd,
    backup: &OwnedFd,
    unit: &Unit,
    new_bin: &str,
) -> Result<Judged, TreeError> {
    let name = unit.as_str();
    let Some(bytes) = tree::read_file(rules, units, name, UNIT_CAP)? else {
        return Ok(Judged::Nothing(UnitOutcome::Missing));
    };
    let text = String::from_utf8_lossy(&bytes);
    let Some(new) = rewrite_text(unit.kind(), &text, new_bin) else {
        let already = text.contains(new_bin) && !text.contains(OLD_BIN);
        return Ok(Judged::Nothing(if already {
            UnitOutcome::Already
        } else {
            UnitOutcome::NotOurs
        }));
    };
    let kept = tree::read_file(rules, backup, name, UNIT_CAP)?.is_some();
    Ok(Judged::Change {
        original: bytes,
        new,
        kept,
    })
}

/// Read and judge everything [`rewrite`] reads for `unit`, and write nothing: a caller that
/// must not fail half way (the services are stopped between the reading and the writing)
/// asks first.
pub fn check(
    rules: &Rules,
    units: &OwnedFd,
    backup: &OwnedFd,
    unit: &Unit,
    new_bin: &str,
) -> Result<(), TreeError> {
    judge(rules, units, backup, unit, new_bin).map(drop)
}

/// Point the unit file `unit` of `units` (the unit directory, open) at the new link,
/// keeping the original in `backup` the first time.
pub fn rewrite(
    rules: &Rules,
    units: &OwnedFd,
    backup: &OwnedFd,
    unit: &Unit,
    new_bin: &str,
) -> Result<UnitOutcome, TreeError> {
    let name = unit.as_str();
    match judge(rules, units, backup, unit, new_bin)? {
        Judged::Nothing(outcome) => Ok(outcome),
        Judged::Change {
            original,
            new,
            kept,
        } => {
            if !kept {
                tree::replace_file(backup, name, 0o600, &original)?;
            }
            tree::replace_file(units, name, 0o644, new.as_bytes())?;
            Ok(UnitOutcome::Rewritten)
        }
    }
}

/// Put the original of `unit` back from `backup`; `false` when none was kept.
pub fn restore(
    rules: &Rules,
    units: &OwnedFd,
    backup: &OwnedFd,
    unit: &Unit,
) -> Result<bool, TreeError> {
    let name = unit.as_str();
    let Some(bytes) = tree::read_file(rules, backup, name, UNIT_CAP)? else {
        return Ok(false);
    };
    tree::replace_file(units, name, 0o644, &bytes)?;
    Ok(true)
}

#[cfg(test)]
#[path = "units_tests.rs"]
mod tests;
