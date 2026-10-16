//! Rules `storage`, `state-store` and `ports` of [`super::modes`]: where a unit's stores
//! are, that the state store's password is in neither the unit nor kbf's command line,
//! and that no two listeners share a port.
//!
//! `storage` (the `storage` mode): the unit gives every flag a storage node needs to
//! serve the farm ([`STORAGE_FLAGS`]: its cold store, its metadata directory, its
//! listen address and the three certificate flags of mutual TLS), and an `s3://` cold
//! store comes with the key file (`EnvironmentFile=<KBF_ETC>/s3.env`): `kbf storage`
//! parses without any of them and then refuses to start, or serves plain text on
//! loopback only. `--hot-store` is not required: without it uploads go straight to the
//! cold store, slower but correct.
//!
//! `state-store` (every mode; the flag is the frontend's). A unit file is readable by
//! every user (0644, and `systemctl cat`), and so is a command line
//! (`/proc/<pid>/cmdline`), so kbf takes `--state-store=postgres://user@host/db` with no
//! password and reads the password from `--state-store-password-file`. The rule refuses:
//!
//! - A `postgres://` or `postgresql://` URI with a password in any directive of the
//!   unit, in any section: a `user:password@` before the host, or a `password=` query
//!   key. The URI is read from the first `://` to the last `@` of the word, so a `:`
//!   there is taken for a password even when it is a port before a path or query
//!   holding an `@` (a false red, never a missed password).
//! - A `--state-store` whose value names a variable (`${VAR}` or `$VAR`): systemd
//!   expands it into the command line it starts kbf with, so whatever the variable
//!   holds is in kbf's argv.
//! - A Postgres `--state-store` without `--state-store-password-file`, or with one that
//!   is neither an absolute path nor `%d/<name>` (the unit's credentials directory) with
//!   a `LoadCredential=<name>:...` or `LoadCredentialEncrypted=<name>:...` to load it.
//! - On the node ([`on_host`]): a password file that is not a plain file, has a group or
//!   other bit (make it 0600), or is not the unit's `User=`'s (root's with no `User=`):
//!   kbf reads it as that user, and no other may. For `%d/<name>`, systemd copies the
//!   credential into a directory only the unit's user reads; the rule reads the mode of
//!   a `LoadCredential=` source given as an absolute path (systemd reads it as root, so
//!   its owner is not checked), and says `SKIP state-store:` for an encrypted one or one
//!   from the credential store.
//!
//! `ports` (every mode): the listeners `--listen`, `--worker-listen`, `--api-listen` and
//! `--storage-listen` ([`LISTEN_FLAGS`]) of one unit are on four ports. Two on one port
//! fail even on two addresses (`127.0.0.1` and the LAN address): one port per listener
//! on a host is what the settings give (`KBF_*_PORT`), and a second listener that binds
//! is an operator's surprise.

use kbf_host::exec::{Exec, Kind};
use kbf_host::path::HostPath;
use kbf_host::user;

use super::credential::{self, OnHost};
use crate::unit::{UnitFile, WHITESPACE};

/// The flag that names the file holding the state store's Postgres password.
pub const PASSWORD_FILE: &str = "state-store-password-file";

/// The listener flags of kbf's modes, without `--` (rule `ports`).
pub const LISTEN_FLAGS: [&str; 4] = ["listen", "worker-listen", "api-listen", "storage-listen"];

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
    let words = exec_words(unit);
    let mut out = Vec::new();
    let missing: Vec<String> = STORAGE_FLAGS
        .iter()
        .filter(|f| flag(&words, f).is_none_or(str::is_empty))
        .map(|f| format!("--{f}"))
        .collect();
    if !missing.is_empty() {
        out.push(format!(
            "gives no {}: a storage node keeps its blobs in a cold store and its index in a \
             metadata directory, and serves mutual TLS on its listen address",
            missing.join(" ")
        ));
    }
    let s3 = flag(&words, "cold-store").is_some_and(|v| v.starts_with("s3://"));
    let keyed = credential::given(unit)
        .files
        .iter()
        .any(|f| credential::names_key(f, key));
    if s3 && !keyed {
        out.push(format!(
            "has an s3:// cold store and no EnvironmentFile={}: kbf storage starts without \
             the bucket's key pair and refuses",
            key.display()
        ));
    }
    out
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

/// Whether a `--state-store` value is a Postgres URL.
fn is_postgres(value: &str) -> bool {
    value.starts_with("postgres://") || value.starts_with("postgresql://")
}

/// The `LoadCredential=` or `LoadCredentialEncrypted=` value that loads the credential
/// `name`, the last one when several do: its key and its source (after the `:`, empty
/// when there is none).
fn loads(unit: &UnitFile, name: &str) -> Option<(String, String)> {
    credential::given(unit)
        .credentials
        .iter()
        .rev()
        .find_map(|c| {
            let (key, value) = c.split_once('=').unwrap_or(("", ""));
            let (id, source) = value.split_once(':').unwrap_or((value, ""));
            (key.starts_with("LoadCredential") && id == name)
                .then(|| (key.to_owned(), source.to_owned()))
        })
}

/// Rule `state-store` on the unit alone: its failures.
pub(super) fn state_store(unit: &UnitFile) -> Vec<String> {
    let mut failures = Vec::new();
    for d in unit.directives() {
        let leaks = credential::words(&d.value)
            .iter()
            .any(|w| postgres_uris(w).any(has_password));
        if leaks {
            failures.push(format!(
                "{}= holds a postgres URI with a password: a unit file and a command line \
                 are readable by every user; leave the password out of the URI and give \
                 --{PASSWORD_FILE}=<a file of mode 0600, the unit's User='s>",
                d.key
            ));
        }
    }
    let words = exec_words(unit);
    let store = flag(&words, "state-store");
    if let Some(var) = store.and_then(variable) {
        failures.push(format!(
            "--state-store names ${{{var}}}: systemd expands it into kbf's command line, \
             which every user on the node can read in /proc; write the URI, with no \
             password, and give --{PASSWORD_FILE}"
        ));
    }
    if !store.is_some_and(is_postgres) {
        return failures;
    }
    match flag(&words, PASSWORD_FILE).filter(|f| !f.is_empty()) {
        None => failures.push(format!(
            "--state-store is a postgres URI and the unit gives no --{PASSWORD_FILE}: the \
             password goes in a file of mode 0600, the unit's User='s"
        )),
        Some(file) => match file.strip_prefix("%d/") {
            Some(name) if loads(unit, name).is_none() => failures.push(format!(
                "--{PASSWORD_FILE}={file} names the credential {name}, and no \
                 LoadCredential={name}:<file> loads it"
            )),
            Some(_) => {}
            None if HostPath::new(file).is_err() => failures.push(format!(
                "--{PASSWORD_FILE}={file} is neither an absolute path nor %d/<credential>"
            )),
            None => {}
        },
    }
    failures
}

/// Rule `state-store` on the node: the file `--state-store-password-file` names is a
/// plain file of the unit's user with no group or other bit (for a `%d/<name>`
/// credential, the source a `LoadCredential=` reads, with no group or other bit).
pub(super) fn on_host<E: Exec>(exec: &E, unit: &UnitFile) -> OnHost {
    let mut out = OnHost::default();
    let words = exec_words(unit);
    let Some(file) = flag(&words, PASSWORD_FILE)
        .filter(|_| flag(&words, "state-store").is_some_and(is_postgres))
    else {
        return out;
    };
    // The file, and the user who must own it (`None`: systemd reads it as root).
    let user = unit
        .get_in("Service", "User")
        .map(|u| u.trim_matches(WHITESPACE))
        .filter(|u| !u.is_empty());
    let (path, owner) = match file.strip_prefix("%d/") {
        Some(name) => match loads(unit, name) {
            Some((key, source)) if key == "LoadCredential" && source.starts_with('/') => {
                (source, None)
            }
            Some(_) => {
                out.notes.push(format!(
                    "SKIP state-store: the credential {name} is encrypted or comes from the \
                     credential store, so its source's mode was not checked"
                ));
                return out;
            }
            None => return out,
        },
        None => (file.to_owned(), Some(user.unwrap_or("root"))),
    };
    let Ok(path) = HostPath::new(&path) else {
        return out;
    };
    let meta = match exec.stat(&path) {
        Ok(Some(m)) => m,
        Ok(None) => {
            out.notes.push(format!(
                "SKIP state-store: no password file at {path} yet, so its owner and mode were \
                 not checked"
            ));
            return out;
        }
        Err(e) => {
            out.notes.push(format!(
                "SKIP state-store: cannot read {path} ({e}), so its owner and mode were not \
                 checked"
            ));
            return out;
        }
    };
    if meta.kind != Kind::File {
        out.failures.push(format!(
            "{path}, the state store's password file, is not a plain file (a link or a \
             directory)"
        ));
        return out;
    }
    let bits = meta.perms.bits();
    if bits & 0o077 != 0 {
        out.failures.push(format!(
            "{path}, the state store's password file, is mode {bits:04o}: other users can \
             read the password; make it 0600"
        ));
    }
    let Some(owner) = owner else {
        return out;
    };
    let uid = match owner {
        "root" => Ok(Some(0)),
        name => user::lookup(exec, name).map(|u| u.map(|u| u.uid)),
    };
    match uid {
        Ok(Some(uid)) if uid == meta.owner.uid => {}
        Ok(Some(uid)) => out.failures.push(format!(
            "{path}, the state store's password file, is uid {}'s, not {owner}'s (uid \
             {uid}): kbf reads it as {owner}, and no other user may",
            meta.owner.uid
        )),
        Ok(None) => out
            .failures
            .push(format!("User={owner} is not a user on this node")),
        Err(e) => out.notes.push(format!(
            "SKIP state-store: cannot look up User={owner} ({e}), so the owner of {path} was \
             not checked"
        )),
    }
    out
}

/// Rule `ports`: the listener flags of `unit` that share a port with an earlier one.
pub(super) fn ports(unit: &UnitFile) -> Vec<String> {
    let words = exec_words(unit);
    let mut seen: Vec<(&str, &str)> = Vec::new();
    let mut out = Vec::new();
    for f in LISTEN_FLAGS {
        let Some((_, port)) = flag(&words, f).and_then(|v| v.rsplit_once(':')) else {
            continue;
        };
        match seen.iter().find(|(p, _)| *p == port) {
            Some((_, first)) => out.push(format!(
                "--{first} and --{f} are both on port {port}: give each listener its own \
                 (KBF_REAPI_PORT, KBF_WORKER_PORT, KBF_API_PORT, KBF_STORAGE_PORT)"
            )),
            None => seen.push((port, f)),
        }
    }
    out
}

#[cfg(test)]
#[path = "stores_tests.rs"]
mod tests;
