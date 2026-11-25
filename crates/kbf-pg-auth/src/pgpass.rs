//! libpq's password file (`~/.pgpass`, or the file `PGPASSFILE` names), read as libpq reads it:
//! `host:port:database:user:password` lines, `*` for any value, `\` escaping the next character,
//! `#` lines as comments, and the first line that matches giving the password.

use std::fmt;
use std::io::Read as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use postgres::config::Host;
use rustix::fs::OFlags;
use zeroize::Zeroizing;

use crate::{Found, Source};

/// The port a URL that names none connects to.
const DEFAULT_PORT: u16 = 5432;

/// libpq's password file was there and was not read: why, for the caller's log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    /// The file.
    pub path: PathBuf,
    /// Why it was skipped.
    pub why: String,
}

impl fmt::Display for Skipped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} is skipped, as libpq skips it: {}",
            self.path.display(),
            self.why
        )
    }
}

/// What a lookup that found no password met.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Miss {
    /// The file was skipped.
    Skipped(Skipped),
    /// The hosts of the URL match lines with different passwords.
    HostsDiffer,
}

/// One server the client may try: what its line must match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Target {
    host: Vec<u8>,
    port: Vec<u8>,
    database: Vec<u8>,
    user: Vec<u8>,
}

/// The servers `url` names, each with the role and database it is logged in to: the role the
/// URL names, else `user` (the effective user's name, as the client takes it); the database the
/// URL names, else the role's name. `url` is one [`kbf_state::postgres::PostgresConfig::parse`]
/// took; its TLS parameters, which this client does not read, are left out.
pub(crate) fn targets(url: &str, user: Option<&str>) -> Vec<Target> {
    let url = url.trim();
    let (head, query) = url.split_once('?').unwrap_or((url, ""));
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| {
            let key = pair.split_once('=').map_or(*pair, |(key, _)| key);
            !matches!(key, "" | "sslmode" | "sslrootcert")
        })
        .collect();
    let text = if kept.is_empty() {
        head.to_owned()
    } else {
        format!("{head}?{}", kept.join("&"))
    };
    let Ok(config) = text.parse::<postgres::Config>() else {
        return Vec::new();
    };
    let user = config.get_user().or(user).unwrap_or_default();
    let database = config.get_dbname().unwrap_or(user);
    let ports = config.get_ports();
    config
        .get_hosts()
        .iter()
        .enumerate()
        .map(|(i, host)| Target {
            host: match host {
                Host::Tcp(name) => name.as_bytes().to_vec(),
                Host::Unix(dir) => dir.as_os_str().as_bytes().to_vec(),
            },
            port: ports
                .get(i)
                .or(ports.first())
                .copied()
                .unwrap_or(DEFAULT_PORT)
                .to_string()
                .into_bytes(),
            database: database.as_bytes().to_vec(),
            user: user.as_bytes().to_vec(),
        })
        .collect()
}

/// The password the file at `path` gives every target, with the line that gave it.
/// `Ok(None)` when the file is missing or no line matches any target.
///
/// # Errors
/// [`Miss::Skipped`] for a file libpq would skip; [`Miss::HostsDiffer`] when the targets do not
/// all match lines with the same password.
pub(crate) fn lookup(path: &Path, targets: &[Target]) -> Result<Option<Found>, Miss> {
    let skip = |why: String| {
        Miss::Skipped(Skipped {
            path: path.to_owned(),
            why,
        })
    };
    // One mapping for every step: only the open fails in practice.
    let unreadable = |e: std::io::Error| skip(format!("it cannot be read: {e}"));
    let flags = OFlags::NONBLOCK.bits().cast_signed();
    let mut open = std::fs::OpenOptions::new();
    let mut file = match open.read(true).custom_flags(flags).open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(unreadable(e)),
    };
    let meta = file.metadata().map_err(unreadable)?;
    if !meta.is_file() {
        return Err(skip("it is not a regular file".to_owned()));
    }
    let mode = meta.mode() & 0o7777;
    if mode & 0o077 != 0 {
        return Err(skip(format!(
            "its group or others have access (mode {mode:04o}); it must be 0600 or less"
        )));
    }
    let mut text = Zeroizing::new(Vec::new());
    file.read_to_end(&mut text).map_err(unreadable)?;
    // Each host's password with its escapes undone, so that two lines that spell one password
    // differently (or say more after it) agree.
    let found: Vec<Option<(Zeroizing<Vec<u8>>, usize)>> = targets
        .iter()
        .map(|t| matching(&text, t).map(|(field, line)| (Zeroizing::new(unescape(field)), line)))
        .collect();
    let Some(Some((password, line))) = found.first().cloned() else {
        return if found.iter().all(Option::is_none) {
            Ok(None)
        } else {
            Err(Miss::HostsDiffer)
        };
    };
    if found
        .iter()
        .any(|other| other.as_ref().is_none_or(|(p, _)| *p != password))
    {
        return Err(Miss::HostsDiffer);
    }
    let source = Source::Passfile {
        path: path.to_owned(),
        line,
    };
    Ok(Some((password, source)))
}

/// The password field (escapes not yet undone) of the first line of `text` that matches
/// `target`, with its line number from 1.
fn matching<'t>(text: &'t [u8], target: &Target) -> Option<(&'t [u8], usize)> {
    text.split(|&b| b == b'\n')
        .enumerate()
        .find_map(|(i, line)| {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.is_empty() || line.starts_with(b"#") {
                return None;
            }
            let rest = field(line, &target.host)?;
            let rest = field(rest, &target.port)?;
            let rest = field(rest, &target.database)?;
            let rest = field(rest, &target.user)?;
            Some((rest, i + 1))
        })
}

/// What follows the field at the start of `line` when it matches `token` (libpq's
/// `pwdfMatchesString`): the field is `*`, or equals `token` once each `\` has escaped the
/// character after it. A field must end with an unescaped `:`.
fn field<'l>(line: &'l [u8], token: &[u8]) -> Option<&'l [u8]> {
    if let Some(rest) = line.strip_prefix(b"*:") {
        return Some(rest);
    }
    let mut at = 0;
    let mut matched = 0;
    while at < line.len() {
        let escaped = line[at] == b'\\';
        if escaped {
            at += 1;
        }
        let byte = *line.get(at)?;
        if byte == b':' && !escaped && matched == token.len() {
            return Some(&line[at + 1..]);
        }
        if token.get(matched) != Some(&byte) {
            return None;
        }
        at += 1;
        matched += 1;
    }
    None
}

/// The password field with its escapes undone, up to the first unescaped `:` (libpq's
/// de-escaping: a `\` before another character stands for that character, a last `\` for itself).
fn unescape(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut bytes = field.iter().copied().peekable();
    while let Some(byte) = bytes.next() {
        match (byte, bytes.peek()) {
            (b':', _) => break,
            (b'\\', Some(&next)) => {
                out.push(next);
                bytes.next();
            }
            (byte, _) => out.push(byte),
        }
    }
    out
}

#[cfg(test)]
#[path = "pgpass_tests.rs"]
mod tests;
