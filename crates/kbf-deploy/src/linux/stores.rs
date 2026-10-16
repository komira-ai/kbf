//! Rules `storage` and `state-store` of [`super::modes`]: where a unit's stores are, and
//! that the state store's password is never in the unit.
//!
//! `storage` (the `storage` mode): the unit gives every flag a storage node needs to
//! serve the farm ([`STORAGE_FLAGS`]: its cold store, its metadata directory, its
//! listen address and the three certificate flags of mutual TLS), and an `s3://` cold
//! store comes with the key file (`EnvironmentFile=<KBF_ETC>/s3.env`): `kbf storage`
//! parses without any of them and then refuses to start, or serves plain text on
//! loopback only. `--hot-store` is not required: without it uploads go straight to the
//! cold store, slower but correct.
//!
//! `state-store` (every mode; the flag is the frontend's and all-in-one's):
//!
//! - No directive of the unit, in any section, holds a `postgres://` or
//!   `postgresql://` URI with a password: a `user:password@` before the host, or a
//!   `password=` query key. A unit file is readable by every user (0644, and
//!   `systemctl cat`), and so is a command line (`/proc/<pid>/cmdline`). The URI is
//!   read from the first `://` to the last `@` of the word, so a `:` there is taken for
//!   a password even when it is a port before a path or query holding an `@` (a false
//!   red, never a missed password).
//! - A `--state-store` whose value names a variable (`${VAR}` or `$VAR`) needs an
//!   `EnvironmentFile=` to set it, and on the node ([`on_host`]) each file that sets
//!   the variable has no group or other bit (mode 0600): it holds the password.
//!
//! What `state-store` cannot change: systemd expands `${VAR}` into the command line it
//! starts kbf with, so the URI is in kbf's argv, which other users on the node can read
//! in `/proc` unless it is mounted `hidepid=2`. The rule keeps the password out of the
//! unit file and the journal's unit text; out of argv needs kbf to read the URI from a
//! file, which it does not today. Every check of a unit that does this prints a
//! `WARN state-store:` line saying so.

use kbf_host::exec::{Entry, Exec};

use super::credential::{self, OnHost};
use super::glob;
use crate::unit::UnitFile;

/// The flags rule `storage` requires of a storage node's unit, without `--`.
pub const STORAGE_FLAGS: [&str; 6] = [
    "cold-store",
    "meta-dir",
    "storage-listen",
    "storage-tls-cert",
    "storage-tls-key",
    "storage-client-ca",
];

/// The words of the last `ExecStart=`, unquoted as systemd unquotes them.
fn exec_words(unit: &UnitFile) -> Vec<String> {
    unit.get("ExecStart")
        .map(credential::words)
        .unwrap_or_default()
}

/// The last value `words` give `--<name>`, as `--<name>=V` or `--<name> V`.
fn flag<'a>(words: &'a [String], name: &str) -> Option<&'a str> {
    let long = format!("--{name}");
    let mut found = None;
    let mut it = words.iter();
    while let Some(w) = it.next() {
        if w == &long {
            found = it.next().map(String::as_str);
        } else if let Some(v) = w
            .strip_prefix(long.as_str())
            .and_then(|r| r.strip_prefix('='))
        {
            found = Some(v);
        }
    }
    found
}

/// Rule `storage`: how the unit of a storage node falls short, one line per way.
pub(super) fn storage(unit: &UnitFile, key: &std::path::Path) -> Vec<String> {
    let _ = (unit, key);
    Vec::new()
}

/// Whether `url`, which starts with a `postgres://` or `postgresql://` scheme, carries
/// a password (see the module docs for how it is read).
fn has_password(url: &str) -> bool {
    let rest = url.split_once("://").map_or("", |(_, r)| r);
    let userinfo = rest.rfind('@').map(|at| &rest[..at]);
    let query = rest.split_once('?').map(|(_, q)| q);
    userinfo.is_some_and(|u| u.contains(':'))
        || query.is_some_and(|q| q.split('&').any(|kv| kv.starts_with("password=")))
}

/// The `postgres://` and `postgresql://` URIs in `word`, each to the end of the word.
fn postgres_uris(word: &str) -> impl Iterator<Item = &str> {
    word.match_indices("postgres")
        .map(|(i, _)| &word[i..])
        .filter(|w| w.starts_with("postgres://") || w.starts_with("postgresql://"))
}

/// The variable a `--state-store` value names: `${VAR}` or `$VAR` anywhere in it.
fn variable(value: &str) -> Option<&str> {
    let after = &value[value.find('$')? + 1..];
    let name = match after.strip_prefix('{') {
        Some(braced) => &braced[..braced.find('}')?],
        None => {
            let end = after
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(after.len());
            &after[..end]
        }
    };
    (!name.is_empty()).then_some(name)
}

/// Rule `state-store` on the unit alone: its failures, and the `WARN state-store:`
/// line when `--state-store` names a variable.
pub(super) fn state_store(unit: &UnitFile) -> (Vec<String>, Option<String>) {
    let _ = unit;
    (Vec::new(), None)
}

/// Rule `state-store` on the node: each environment file of the unit that sets the
/// variable `--state-store` names has no group or other bit.
pub(super) fn on_host<E: Exec>(exec: &E, unit: &UnitFile) -> OnHost {
    let _ = (exec, unit);
    OnHost::default()
}

#[cfg(test)]
#[path = "stores_tests.rs"]
mod tests;
