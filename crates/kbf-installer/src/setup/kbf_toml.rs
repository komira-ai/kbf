//! The `/etc/kbf/kbf.toml` that `setup` writes: the host's mode, the frontend URL and the
//! enrollment token's path (the keys enrollment reads, `kbf-enroll`'s `KEYS`), and on a host
//! that serves the operator API, the path of its token (`api-token-file`, group `tls`) and, on a
//! host that keeps its state in Postgres, the database (`state-store`, group `scheduler`). A
//! `frontend` or `all-in-one` host also has the listener, certificate, storage and daemon-half
//! keys the flags of [`super::kbf_settings`] give. Everything else in the file is the mode's own
//! flags, which the operator or the package adds; `setup` never writes an update or installer
//! key (those are `installer.toml`).
//!
//! A file that exists is not overwritten blindly: when every key in it is one `setup`
//! writes, `setup` rewrites it; when it holds other settings, `setup` leaves it alone if
//! its own keys already say what the flags say, and refuses (exit 5) if they differ, naming
//! them, since rewriting would lose settings it cannot see. What counts as another setting is
//! relative to this run: a key is `setup`'s when it is one of the five above or one the flags
//! of this run give, so a file that holds a listener `setup` has no flag for in this run is
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
    /// The listener, certificate, storage and daemon-half settings of this run.
    pub extra: &'a [Setting],
}

/// The tables of the file, in the order they are written. A table not listed follows them.
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
            "# Written by `kbf-installer setup`. The mode's other settings are added below the\n\
             # keys it writes; run setup again to change these.\n",
        );
        out.push_str(&format!("mode = \"{}\"\n", self.mode));
        let mut tables: Vec<&str> = TABLE_ORDER.to_vec();
        for (at, _, _) in &keys {
            if !at.is_empty() && !tables.contains(at) {
                tables.push(*at);
            }
        }
        for at in tables {
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
/// always write, or one of `ours.extra`.
fn has_other_settings(table: &Table, ours: &Ours<'_>) -> bool {
    const MINE: [(&str, &str); 5] = [
        ("", "mode"),
        ("frontend", "url"),
        ("enroll", "token_file"),
        ("tls", "api-token-file"),
        ("scheduler", "state-store"),
    ];
    let mine = |at: &str, key: &str| {
        MINE.contains(&(at, key)) || ours.extra.iter().any(|s| s.table == at && s.key == key)
    };
    table.iter().any(|(k, v)| match v {
        Value::Table(inner) => {
            let known = MINE.iter().any(|(t, _)| *t == k.as_str())
                || ours.extra.iter().any(|s| s.table == k.as_str());
            !known || inner.keys().any(|key| !mine(k, key))
        }
        _ => !mine("", k),
    })
}

/// Decide what `setup` does with the existing file `current` (`None`: there is none).
pub fn decide(current: Option<&str>, ours: &Ours<'_>) -> Decision {
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
                super::paths::KBF_TOML,
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
        Decision::Refuse(format!(
            "{} holds other settings and these keys differ from the flags ({}): edit them by hand",
            super::paths::KBF_TOML,
            differ.join("; ")
        ))
    }
}
