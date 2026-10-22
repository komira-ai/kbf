//! The `kbf.toml` (`/etc/kbf`, `/usr/local/etc/kbf` on a Mac) that `setup` writes: the host's mode, the frontend URL and the
//! enrollment token's path (the keys enrollment reads, `kbf-enroll`'s `KEYS`), and on a host
//! that serves the operator API, the path of its token (`api-token-file`, group `tls`) and, on a
//! host that keeps its state in Postgres, the database (`state-store`, group `scheduler`). A
//! `frontend` or `all-in-one` host also has the listener, certificate, storage and daemon-half
//! keys the flags of [`super::kbf_settings`] give. Everything else in the file is the mode's own
//! flags, which the operator or the package adds. A host whose daemon takes update orders
//! (`daemon`, `storage-and-daemon`) also has `[daemon] update-dir` and `orders-dir`, the
//! installer's directories ([`crate::layout::Layout`]): without them the node updates by hand
//! and ignores every order. `all-in-one` has neither, as its in-process daemon refuses
//! `--update-dir`. The installer's own settings are `installer.toml`, never here.
//!
//! An `all-in-one` host has two such files, both rendered and decided by this module: `kbf.toml`
//! is the sibling's (`mode = "storage-and-daemon"`, no enrollment table, so `kbf` reads it as
//! plain flags) and `frontend.toml` is the frontend children's (`mode = "frontend"`, the
//! operator token and the state database).
//!
//! A file that exists is not overwritten blindly: when every key in it is one `setup`
//! writes, `setup` rewrites it; when it holds other settings, `setup` leaves it alone if
//! its own keys already say what the flags say, and refuses (exit 5) if they differ, naming
//! them, since rewriting would lose settings it cannot see. What counts as another setting is
//! relative to this run: a key is `setup`'s when it is one of the five above, the update
//! directories of a mode that has them, or one the flags of this run give, so a file that holds a listener `setup` has no flag for in this run is
//! the operator's and is never rewritten.

use toml::{Table, Value};

use super::kbf_settings::Setting;

/// What `setup` says goes in the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ours<'a> {
    pub mode: &'static str,
    pub frontend_url: Option<&'a str>,
    pub token_file: Option<&'a str>,
    pub api_token_file: Option<&'a str>,
    /// The state database of a frontend or all-in-one host: `[scheduler] state-store`.
    pub state_store: Option<&'a str>,
    /// `[daemon] update-dir` and `orders-dir` of a host whose daemon takes orders, both or
    /// neither.
    pub update: Option<UpdateDirs<'a>>,
    /// The listener, certificate, storage and daemon-half settings of this run.
    pub extra: &'a [Setting],
}

/// The installer's directories as the node's `kbf.toml` names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UpdateDirs<'a> {
    /// Root's directory, where the installer writes `status.json`.
    pub update_dir: &'a str,
    /// The order spool, the node's.
    pub orders_dir: &'a str,
}

/// The keys of [`UpdateDirs`], as `(table, key)`.
pub const UPDATE_KEYS: [(&str, &str); 2] = [("daemon", "update-dir"), ("daemon", "orders-dir")];

/// Whether a Mac's `kbf.toml` carries the node's directories as `kbf-mac-provision` renders
/// them: `[daemon] update-dir` is there and is `update_dir`, and `orders-dir` is absent (the
/// node derives `<update-dir>/orders`, `kbf_update::Config::new`) or is `orders_dir`. The
/// installer never writes these on a Mac. The message names the key and prints no path.
///
/// # Errors
/// What is missing or differs, one line.
pub fn check_provisioned(table: &Table, update_dir: &str, orders_dir: &str) -> Result<(), String> {
    let refuse = |why: String| {
        Err(format!(
            "kbf.toml {why}: kbf-mac-provision renders it (run it again); the installer does not write it on a Mac"
        ))
    };
    for (key, want, required) in [
        ("update-dir", update_dir, true),
        ("orders-dir", orders_dir, false),
    ] {
        match lookup(table, "daemon", key) {
            None if required => return refuse(format!("has no [daemon] {key}")),
            Some(v) => match v.as_str() {
                None => return refuse(format!("[daemon] {key} is not text")),
                Some(have) if have != want => {
                    return refuse(format!("[daemon] {key} is not the installer's directory"));
                }
                Some(_) => {}
            },
            None => {}
        }
    }
    Ok(())
}

/// Whether `text` is a `kbf.toml` whose top-level `mode` is `all-in-one`: the layout of one
/// process for everything, which an all-in-one host no longer has. A text that is not TOML says
/// no (`decide` names that fault).
pub fn says_all_in_one(text: &str) -> bool {
    let Ok(table) = text.parse::<Table>() else {
        return false;
    };
    lookup(&table, "", "mode").and_then(Value::as_str) == Some("all-in-one")
}

/// The tables of the file, in the order they are written: every table a setting can be in
/// (a test renders each mode's file and compares it with its golden, so a missing one fails).
const TABLE_ORDER: [&str; 9] = [
    "frontend",
    "enroll",
    "listener",
    "tls",
    "storage-client",
    "stores",
    "identity",
    "daemon",
    "scheduler",
];

impl Ours<'_> {
    /// The canonical file: `mode`, then each table in [`TABLE_ORDER`] with its keys in the order
    /// of [`Ours::keys`]. Every value is validated text with no character that needs a TOML
    /// escape (a URL of plain characters, an absolute path, a mode name).
    pub fn render(&self) -> String {
        let keys = self.keys();
        let mut out = String::from(
            "# Written by `kbf-installer setup`; run setup again to change these keys. Another setting\n\
             # goes into the table it belongs to (add the key there, never a second table of the same name).\n",
        );
        out.push_str(&format!("mode = \"{}\"\n", self.mode));
        for at in TABLE_ORDER {
            let mut first = true;
            for (_, key, value) in keys.iter().filter(|(t, _, _)| *t == at) {
                if first {
                    out.push_str(&format!("\n[{at}]\n"));
                    first = false;
                }
                out.push_str(&format!("{key} = \"{value}\"\n"));
            }
        }
        out
    }

    /// The keys, as `(table, key, value)`; `table` is empty for a top-level key.
    fn keys(&self) -> Vec<(&'static str, &'static str, String)> {
        let mut keys = vec![("", "mode", self.mode.to_owned())];
        let optional = [
            ("frontend", "url", self.frontend_url),
            ("enroll", "token_file", self.token_file),
            ("tls", "api-token-file", self.api_token_file),
            ("scheduler", "state-store", self.state_store),
        ];
        for (table, key, value) in optional {
            if let Some(v) = value {
                keys.push((table, key, v.to_owned()));
            }
        }
        if let Some(dirs) = &self.update {
            keys.push(("daemon", "update-dir", dirs.update_dir.to_owned()));
            keys.push(("daemon", "orders-dir", dirs.orders_dir.to_owned()));
        }
        keys.extend(self.extra.iter().map(|s| (s.table, s.key, s.value.clone())));
        keys
    }
}

/// What to do about the file that is there.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// Make the file this text.
    Write(String),
    /// Leave it as it is: it holds other settings, and the keys setup writes match.
    Keep,
    /// Only a person can go on; the reason, one line.
    Refuse(String),
}

fn lookup<'t>(table: &'t Table, at: &str, key: &str) -> Option<&'t Value> {
    if at.is_empty() {
        table.get(key)
    } else {
        table.get(at)?.as_table()?.get(key)
    }
}

/// Whether `table` holds a key that is not one `setup` writes in this run: the five it can
/// always write, the update directories if this run has them, or one of `ours.extra`.
fn has_other_settings(table: &Table, ours: &Ours<'_>) -> bool {
    const MINE: [(&str, &str); 5] = [
        ("", "mode"),
        ("frontend", "url"),
        ("enroll", "token_file"),
        ("tls", "api-token-file"),
        ("scheduler", "state-store"),
    ];
    let update = if ours.update.is_some() {
        UPDATE_KEYS.as_slice()
    } else {
        &[]
    };
    let mine = |at: &str, key: &str| {
        MINE.contains(&(at, key))
            || update.contains(&(at, key))
            || ours.extra.iter().any(|s| s.table == at && s.key == key)
    };
    table.iter().any(|(k, v)| match v {
        Value::Table(inner) => {
            let known = MINE.iter().any(|(t, _)| *t == k.as_str())
                || update.iter().any(|(t, _)| *t == k.as_str())
                || ours.extra.iter().any(|s| s.table == k.as_str());
            !known || inner.keys().any(|key| !mine(k, key))
        }
        _ => !mine("", k),
    })
}

/// Decide what `setup` does with the existing file `current` (`None`: there is none), which
/// is at `path` (named in what it refuses).
pub fn decide(current: Option<&str>, ours: &Ours<'_>, path: &str) -> Decision {
    let wanted = ours.render();
    let Some(text) = current else {
        return Decision::Write(wanted);
    };
    if text == wanted {
        return Decision::Write(wanted);
    }
    let table: Table = match text.parse() {
        Ok(t) => t,
        Err(e) => {
            return Decision::Refuse(format!(
                "{} is not valid TOML ({}): fix it or remove it, then run setup again",
                path,
                crate::installer_toml::redact(e.message())
            ));
        }
    };
    if !has_other_settings(&table, ours) {
        return Decision::Write(wanted);
    }
    let differ: Vec<String> = ours
        .keys()
        .into_iter()
        .filter(|(at, key, value)| lookup(&table, at, key).and_then(Value::as_str) != Some(value))
        .map(|(at, key, value)| {
            if at.is_empty() {
                format!("{key} should be {value:?}")
            } else if ours.extra.iter().any(|s| s.table == at && s.key == key) {
                // Paths and URLs the operator typed stay out of the message, as the database
                // URL does: the key is named and the flag it came from is the way to see it.
                format!("[{at}] {key} differs from its flag")
            } else if UPDATE_KEYS.contains(&(at, key)) {
                // A path of the installer's layout, not a secret; the key is named all the same.
                format!("[{at}] {key} differs from the installer's directory")
            } else if at == "scheduler" {
                // The database URL is never printed, not even one without a password.
                format!("[{at}] {key} should be the --database URL")
            } else {
                format!("[{at}] {key} should be {value:?}")
            }
        })
        .collect();
    if differ.is_empty() {
        Decision::Keep
    } else {
        let api = differ
            .iter()
            .any(|d| d.starts_with("[listener] api-listen"));
        let way = if api {
            "edit them by hand, or pass --api-listen with the address in the file (a run without it \
             means the default 127.0.0.1:8982)"
        } else {
            "edit them by hand"
        };
        Decision::Refuse(format!(
            "{} holds other settings and these keys differ from the flags ({}): {way}",
            path,
            differ.join("; ")
        ))
    }
}
