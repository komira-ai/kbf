//! Rules `storage`, `state-store` and `ports` of [`super::modes`]: where a unit's stores
//! are, that the state store is a password-free Postgres URI (a SQLite path in the
//! all-in-one's unit), and that no two listeners share a port.
//!
//! `storage` (the `storage` mode): the unit gives every flag a storage node needs to
//! serve the farm ([`STORAGE_FLAGS`]: its cold store, its metadata directory, its
//! listen address and the three certificate flags of mutual TLS), and an `s3://` cold
//! store comes with the key file (`EnvironmentFile=<KBF_ETC>/s3.env`): `kbf storage`
//! parses without any of them and then refuses to start, or serves plain text on
//! loopback only. `--hot-store` is not required: without it uploads go straight to the
//! cold store, slower but correct.
//!
//! `state-store` (every mode; the flag is the frontend's). The frontend's state store is
//! Postgres and SQLite is dropped for it (CEO ruling); the all-in-one unit still names
//! its SQLite file until the farm is moved, so there a file is judged by its path. The
//! rule refuses:
//!
//! - In the frontend's unit, any value that is not a `postgres://` or `postgresql://`
//!   URI: a `sqlite://` file, another scheme, or none.
//! - In another unit, a scheme that is neither Postgres nor `sqlite://`, and a
//!   `sqlite://` value whose path is empty, relative or not normal (`//`, `.` or `..`).
//! - A Postgres URI that carries a password (user information with a colon, or a
//!   `password` parameter): kbf refuses one at start, and the command line is readable.
//! - A value that names a variable (`${VAR}` or `$VAR`): systemd expands it into the
//!   command line it starts kbf with, so the rule cannot read the store it names, and
//!   whatever the variable holds is in kbf's argv, which every user can read.
//!
//! No message echoes the value: a mistyped one may carry a password. A unit with no
//! `--state-store` passes (the modes that hold no state store give none).
//!
//! `ports` (every mode): the listeners `--listen`, `--worker-listen`, `--api-listen` and
//! `--storage-listen` ([`LISTEN_FLAGS`]) of one unit are on four ports. Two on one port
//! fail even on two addresses (`127.0.0.1` and the LAN address): one port per listener
//! on a host is what the settings give (`KBF_*_PORT`), and a second listener that binds
//! is an operator's surprise.

use kbf_host::path::HostPath;

use super::Mode;
use super::credential;
use crate::unit::UnitFile;

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

/// Rule `storage`: how the unit of a storage node falls short, one line per way. With
/// `exact`, the unit that holds the key beside its builds (`storage-and-daemon`), the key
/// file must be named exactly (`EnvironmentFile=<key>`, as written), not by a wildcard or
/// with a `-`.
pub(super) fn storage(unit: &UnitFile, key: &std::path::Path, exact: bool) -> Vec<String> {
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
    let keyed = credential::given(unit).files.iter().any(|f| {
        if exact {
            credential::is_exactly(f, key)
        } else {
            credential::names_key(f, key)
        }
    });
    if s3 && !keyed {
        out.push(format!(
            "has an s3:// cold store and no EnvironmentFile={}: kbf storage starts without \
             the bucket's key pair and refuses",
            key.display()
        ));
    }
    out
}

/// Rule `meta-dir`: the unit's `--meta-dir` is not `--data-dir` or under it. The metadata
/// log is the index of what the store holds (lose it and the blobs are unreachable),
/// and `--data-dir` is the daemon's, which holds the builds' leases and cache. Paths are compared
/// lexically after resolving `.` and `..`; a unit that gives either flag relative is
/// left to rule `storage`'s and the parser's own refusals.
pub(super) fn meta_dir(unit: &UnitFile) -> Vec<String> {
    let words = exec_words(unit);
    let (Some(meta), Some(data)) = (flag(&words, "meta-dir"), flag(&words, "data-dir")) else {
        return Vec::new();
    };
    if !meta.starts_with('/') || !data.starts_with('/') {
        return Vec::new();
    }
    if normal(meta).starts_with(normal(data)) {
        vec![format!(
            "has --meta-dir={meta} under --data-dir={data}: --data-dir is the \
             daemon's, and the metadata log, the index of what the store holds, is not \
             kept there"
        )]
    } else {
        Vec::new()
    }
}

/// The absolute `path` with `..` resolved lexically (`components` already drops a `.`
/// in the middle of a path; a leading one only a relative path has, and the caller
/// refuses those).
fn normal(path: &str) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for c in std::path::Path::new(path).components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
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

/// The part of a Postgres URI after its scheme, or `None` when `value` is not one (or
/// is nothing but the scheme).
fn postgres_rest(value: &str) -> Option<&str> {
    value
        .strip_prefix("postgres://")
        .or_else(|| value.strip_prefix("postgresql://"))
        .filter(|rest| !rest.is_empty())
}

/// Whether a Postgres URI carries a password: a colon in its user information, or a
/// `password` parameter (any case). kbf refuses such a URI, whose command line every
/// user on the node can read.
pub(super) fn has_password(value: &str) -> bool {
    let rest = postgres_rest(value).unwrap_or(value);
    let (before, query) = rest.split_once('?').unwrap_or((rest, ""));
    let authority = before.split('/').next().unwrap_or("");
    let user = authority.rsplit_once('@').map(|(user, _)| user);
    user.is_some_and(|u| u.contains(':'))
        || query.split('&').any(|pair| {
            pair.split('=')
                .next()
                .is_some_and(|k| k.eq_ignore_ascii_case("password"))
        })
}

/// Why a `--state-store` value is refused in the unit of `mode`, or `None`.
fn refusal(value: &str, mode: Mode) -> Option<String> {
    if let Some(var) = variable(value) {
        return Some(format!(
            "--state-store names ${{{var}}}: systemd expands it into kbf's command line, \
             which every user on the node can read in /proc, and the rule cannot read the \
             store it names; write the URI itself"
        ));
    }
    if postgres_rest(value).is_some() {
        return has_password(value).then(|| {
            "--state-store carries a password: kbf's command line is readable by every user \
             on the node, and kbf refuses such a URI; give a socket or a passwordless URI"
                .to_owned()
        });
    }
    let frontend = mode == Mode::Frontend;
    if let Some(path) = value.strip_prefix("sqlite://") {
        if frontend {
            return Some(
                "--state-store is sqlite:// in a frontend unit: the frontend's state store \
                 is Postgres (postgres://<host>/<db>), and SQLite is dropped for it"
                    .to_owned(),
            );
        }
        return HostPath::new(path).is_err().then(|| {
            "--state-store is sqlite:// with an empty, relative or not normal path: give \
             sqlite:///<absolute path>"
                .to_owned()
        });
    }
    Some(
        if frontend {
            "--state-store is not postgres://<host>/<db>: the frontend's state store is Postgres"
        } else {
            "--state-store is neither postgres://<host>/<db> nor sqlite:///<absolute path>"
        }
        .to_owned(),
    )
}

/// Rule `state-store`: its failures (at most one, see the module docs).
pub(super) fn state_store(unit: &UnitFile, mode: Mode) -> Vec<String> {
    let words = exec_words(unit);
    flag(&words, "state-store")
        .and_then(|value| refusal(value, mode))
        .into_iter()
        .collect()
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
