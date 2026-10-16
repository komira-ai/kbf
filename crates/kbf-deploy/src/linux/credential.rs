//! Rule `credential` of [`super::modes`]: the units of a build node do not give it the
//! cold store's key. The rule reads the unit and its drop-ins as systemd applies them
//! (systemd.exec(5)): every assignment in order, an empty one resetting its list.
//!
//! What it refuses, in `[Service]`:
//!
//! - `EnvironmentFile=` naming the key file: its path, after `-`, quotes, `.`, `..` and
//!   repeated `/` are taken off, is `<KBF_ETC>/s3.env` or ends in `/s3.env` wherever
//!   it is, or it is a wildcard (`*`, `?`, `[...]`, as glob(3) matches them) that
//!   matches `<KBF_ETC>/s3.env`.
//! - `Environment=` setting any `AWS_*` variable, with systemd's quoting (several
//!   assignments on a line, single or double quotes around an assignment or inside
//!   one). A build node talks to no AWS-style store, so no `AWS_*` variable has a use
//!   there: the key pair, `AWS_SESSION_TOKEN`, and `AWS_PROFILE`,
//!   `AWS_SHARED_CREDENTIALS_FILE` or `AWS_CONFIG_FILE`, which point an SDK at a key
//!   anywhere.
//! - `PassEnvironment=` naming an `AWS_*` variable (it passes the service manager's).
//! - Any `LoadCredential=`, `LoadCredentialEncrypted=`, `SetCredential=`,
//!   `SetCredentialEncrypted=` or `ImportCredential=`: a systemd credential is opaque to
//!   the rule, and a build node's units need none.
//!
//! On the node ([`on_host`]) it also reads each `EnvironmentFile=` that names one file,
//! and refuses one that sets an `AWS_*` variable (a copy or a hard link of the key under
//! another name) or that is a symbolic link (not followed, so its target is unknown).
//!
//! What it cannot catch:
//!
//! - Offline, a file under another name that holds the key, or links to it, and a
//!   wildcard elsewhere that matches a copy named `s3.env`.
//! - On the node, the files a wildcard `EnvironmentFile=` matches (not listed: a
//!   `SKIP credential:` line says so), and a file the check cannot read (another
//!   `SKIP credential:` line).
//! - `%` specifiers (`%E/kbf/s3.env`): not expanded, so only the last part matches.
//! - Drop-ins outside the unit's own directory (`/run`, `/usr/lib`, the `service.d/`
//!   and prefix drop-ins), and `systemctl set-property` or `edit --runtime` changes.
//! - `UnsetEnvironment=` is not read: a variable it unsets still fails the rule.
//! - The key reaching the node any other way: a file the daemon reads by a flag, the
//!   service manager's own environment without `PassEnvironment=`, or a world-readable
//!   key file.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use kbf_host::exec::{Entry, Exec};
use kbf_host::path::HostPath;

use super::modes::COLD_STORE_KEY_FILE;
use crate::unit::UnitFile;

/// The prefix of the variables an AWS SDK or CLI reads (rule `credential`).
pub const AWS_PREFIX: &str = "AWS_";

/// The keys that pass a systemd credential, in the groups an empty value resets
/// together (systemd's `config_parse_load_credential`, `_set_credential`,
/// `_import_credential`).
const CREDENTIAL_KEYS: [&[&str]; 3] = [
    &["LoadCredential", "LoadCredentialEncrypted"],
    &["SetCredential", "SetCredentialEncrypted"],
    &["ImportCredential"],
];

/// What a unit's `[Service]` gives its processes, every assignment applied in order.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Given {
    /// The variables `Environment=` sets.
    pub set: BTreeSet<String>,
    /// The `EnvironmentFile=` values, as written.
    pub files: Vec<String>,
    /// The variables `PassEnvironment=` passes.
    pub passed: BTreeSet<String>,
    /// Each credential directive, as `Key=value`.
    pub credentials: Vec<String>,
}

/// What `unit` gives, by systemd's rules for each key.
pub(super) fn given(unit: &UnitFile) -> Given {
    let mut g = Given::default();
    let mut creds: [Vec<String>; 3] = Default::default();
    for d in unit.directives().iter().filter(|d| d.section == "Service") {
        let v = d.value.as_str();
        match d.key.as_str() {
            "Environment" if v.is_empty() => g.set.clear(),
            "Environment" => g.set.extend(
                words(v)
                    .into_iter()
                    .filter_map(|w| w.split_once('=').map(|(name, _)| name.to_owned())),
            ),
            "EnvironmentFile" if v.is_empty() => g.files.clear(),
            "EnvironmentFile" => g.files.push(v.to_owned()),
            "PassEnvironment" if v.is_empty() => g.passed.clear(),
            "PassEnvironment" => g.passed.extend(words(v)),
            key => {
                if let Some(i) = CREDENTIAL_KEYS.iter().position(|g| g.contains(&key)) {
                    if v.is_empty() {
                        creds[i].clear();
                    } else {
                        creds[i].push(format!("{key}={v}"));
                    }
                }
            }
        }
    }
    g.credentials = creds.concat();
    g
}

/// How `unit` gives a build node the key whose file is `key`, one line per way.
pub(super) fn key_given(unit: &UnitFile, key: &Path) -> Vec<String> {
    let g = given(unit);
    let mut out: Vec<String> = g
        .files
        .iter()
        .filter(|f| names_key(f, key))
        .map(|f| format!("EnvironmentFile={f}"))
        .collect();
    out.extend(
        g.set
            .iter()
            .filter(|n| n.starts_with(AWS_PREFIX))
            .map(|n| format!("Environment= sets {n}")),
    );
    out.extend(
        g.passed
            .iter()
            .filter(|n| n.starts_with(AWS_PREFIX))
            .map(|n| format!("PassEnvironment= passes {n}")),
    );
    out.extend(
        g.credentials
            .iter()
            .map(|c| format!("{c}, a systemd credential")),
    );
    out
}

/// What reading the node's environment files found: failures and `SKIP` notes.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct OnHost {
    pub failures: Vec<String>,
    pub notes: Vec<String>,
}

/// Rule `credential`'s reading of the files `unit`'s `EnvironmentFile=` lines name, on
/// the host `exec` reads; `key` is the key file, which [`key_given`] reports itself.
pub(super) fn on_host<E: Exec>(exec: &E, unit: &UnitFile, key: &Path) -> OnHost {
    let mut out = OnHost::default();
    for f in given(unit).files.iter().filter(|f| !names_key(f, key)) {
        let path = normalise(bare(f));
        if is_pattern(&path.to_string_lossy()) {
            out.notes.push(format!(
                "SKIP credential: EnvironmentFile={f} is a wildcard; the files it matches \
                 were not read"
            ));
            continue;
        }
        // A relative path is one systemd ignores.
        let Ok(host) = HostPath::new(&path.to_string_lossy()) else {
            continue;
        };
        match exec.inspect(&host) {
            Ok(Some(Entry::File { bytes, .. })) => {
                if let Some(name) = env_file_names(&bytes).find(|n| n.starts_with(AWS_PREFIX)) {
                    out.failures
                        .push(format!("EnvironmentFile={f}, which sets {name}"));
                }
            }
            Ok(Some(Entry::Other)) => out.failures.push(format!(
                "EnvironmentFile={f}, a link or special file the check does not follow \
                 (name the file itself)"
            )),
            Ok(Some(Entry::Dir { .. }) | None) => {}
            Err(e) => out.notes.push(format!(
                "SKIP credential: cannot read EnvironmentFile={f} ({e}), so its variables \
                 were not checked"
            )),
        }
    }
    out
}

/// The variable names an environment file assigns: each line's text before `=`, after
/// an `export ` (systemd ignores the line; the rule is stricter), skipping comments.
fn env_file_names(bytes: &[u8]) -> impl Iterator<Item = String> + '_ {
    bytes.split(|b| *b == b'\n').filter_map(|raw| {
        let line = String::from_utf8_lossy(raw);
        let line = line.trim();
        if line.starts_with('#') || line.starts_with(';') {
            return None;
        }
        let (name, _) = line.split_once('=')?;
        let name = name.trim();
        Some(
            name.strip_prefix("export ")
                .unwrap_or(name)
                .trim()
                .to_owned(),
        )
    })
}

/// Whether the `EnvironmentFile=` value `value` names the key file `key`.
fn names_key(value: &str, key: &Path) -> bool {
    let path = normalise(bare(value));
    let text = path.to_string_lossy();
    let last = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if is_pattern(&text) {
        glob(&text, &key.to_string_lossy())
    } else {
        path == key || last == COLD_STORE_KEY_FILE
    }
}

/// The path an `EnvironmentFile=` value names: without the `-` that makes it optional
/// and without quotes around it (systemd takes neither as part of the path).
fn bare(value: &str) -> &str {
    let v = value.trim();
    let v = v.strip_prefix('-').unwrap_or(v);
    v.trim_matches(|c| c == '"' || c == '\'')
}

/// `path` with `.`, `..` and repeated `/` resolved by its words alone (no link read).
pub(super) fn normalise(path: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for c in Path::new(path).components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

fn is_pattern(text: &str) -> bool {
    text.contains(['*', '?', '['])
}

/// Whether the glob(3) pattern `pattern` matches `path`: `*` and `?` never match `/`,
/// `[...]` (`[!...]` negated) matches one character of a set or range.
fn glob(pattern: &str, path: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = path.chars().collect();
    glob_at(&p, &s)
}

fn glob_at(p: &[char], s: &[char]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some('*') => {
            let stop = s.iter().position(|c| *c == '/').unwrap_or(s.len());
            (0..=stop).any(|n| glob_at(&p[1..], &s[n..]))
        }
        Some('?') => s.first().is_some_and(|c| *c != '/') && glob_at(&p[1..], &s[1..]),
        Some('[') => match (class(&p[1..]), s.first()) {
            (Some((set, len)), Some(c)) => *c != '/' && set(*c) && glob_at(&p[1 + len..], &s[1..]),
            (Some(_), None) => false,
            // A `[` that opens no class is a plain character.
            (None, _) => s.first() == Some(&'[') && glob_at(&p[1..], &s[1..]),
        },
        Some(c) => s.first() == Some(c) && glob_at(&p[1..], &s[1..]),
    }
}

/// The class after a `[`: its test and how many characters it takes up to and with
/// the `]`; `None` when no `]` closes it.
fn class(p: &[char]) -> Option<(impl Fn(char) -> bool, usize)> {
    let negated = matches!(p.first(), Some('!' | '^'));
    let start = usize::from(negated);
    // A `]` first in the class is one of its characters.
    let close = p[start..]
        .iter()
        .skip(1)
        .position(|c| *c == ']')
        .map(|i| start + 1 + i)?;
    let body: Vec<char> = p[start..close].to_vec();
    let test = move |c: char| {
        let mut hit = false;
        let mut i = 0;
        while i < body.len() {
            if i + 2 < body.len() && body[i + 1] == '-' {
                hit |= body[i] <= c && c <= body[i + 2];
                i += 3;
            } else {
                hit |= body[i] == c;
                i += 1;
            }
        }
        hit != negated
    };
    Some((test, close + 1))
}

/// The words of a systemd value, unquoted as systemd.syntax(7) says: whitespace
/// separates words outside quotes, `'...'` and `"..."` group (anywhere in a word), and
/// `\` outside single quotes keeps the next character.
pub(super) fn words(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word: Option<String> = None;
    let mut quote: Option<char> = None;
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (None, c) if c.is_whitespace() => out.extend(word.take()),
            (None, '\'' | '"') => {
                quote = Some(c);
                word.get_or_insert_with(String::new);
            }
            (Some(q), c) if c == q => quote = None,
            (Some('\''), c) => word.get_or_insert_with(String::new).push(c),
            (_, '\\') => {
                let w = word.get_or_insert_with(String::new);
                w.extend(chars.next());
            }
            (_, c) => word.get_or_insert_with(String::new).push(c),
        }
    }
    out.extend(word);
    out
}

#[cfg(test)]
#[path = "credential_tests.rs"]
mod tests;
