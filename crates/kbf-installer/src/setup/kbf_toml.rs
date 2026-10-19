//! The `/etc/kbf/kbf.toml` that `setup` writes: the host's mode, the frontend URL and the
//! enrollment token's path (the keys enrollment reads, `kbf-enroll`'s `KEYS`), and on a host
//! that serves the operator API, the path of its token (`api-token-file`, group `tls`) and, on a
//! host that keeps its state in Postgres, the database (`state-store`, group `scheduler`).
//! Everything else in the file is the mode's own flags, which the operator or the package
//! adds; `setup` never writes an update or installer key (those are `installer.toml`).
//!
//! A file that exists is not overwritten blindly: when every key in it is one `setup`
//! writes, `setup` rewrites it; when it holds other settings, `setup` leaves it alone if
//! its own keys already say what the flags say, and refuses (exit 5) if they differ, naming
//! them, since rewriting would lose settings it cannot see.

use toml::{Table, Value};

/// What `setup` says goes in the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ours<'a> {
    pub mode: &'static str,
    pub frontend_url: Option<&'a str>,
    pub token_file: Option<&'a str>,
    pub api_token_file: Option<&'a str>,
    /// The state database of a frontend or all-in-one host: `[scheduler] state-store`.
    pub state_store: Option<&'a str>,
}

impl Ours<'_> {
    /// The canonical file. Every value is validated text with no character that needs a TOML
    /// escape (a URL of plain characters, an absolute path, a mode name).
    pub fn render(&self) -> String {
        let mut out = String::from(
            "# Written by `kbf-installer setup`. The mode's other settings are added below the\n\
             # keys it writes; run setup again to change these.\n",
        );
        out.push_str(&format!("mode = \"{}\"\n", self.mode));
        if let Some(url) = self.frontend_url {
            out.push_str(&format!("\n[frontend]\nurl = \"{url}\"\n"));
        }
        if let Some(path) = self.token_file {
            out.push_str(&format!("\n[enroll]\ntoken_file = \"{path}\"\n"));
        }
        if let Some(path) = self.api_token_file {
            out.push_str(&format!("\n[tls]\napi-token-file = \"{path}\"\n"));
        }
        if let Some(url) = self.state_store {
            out.push_str(&format!("\n[scheduler]\nstate-store = \"{url}\"\n"));
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

/// Whether `table` holds a key that is not one `setup` might write.
fn has_other_settings(table: &Table) -> bool {
    const MINE: [(&str, &str); 5] = [
        ("", "mode"),
        ("frontend", "url"),
        ("enroll", "token_file"),
        ("tls", "api-token-file"),
        ("scheduler", "state-store"),
    ];
    table.iter().any(|(k, v)| match v {
        Value::Table(inner) => {
            !MINE.iter().any(|(t, _)| *t == k.as_str())
                || inner
                    .keys()
                    .any(|key| !MINE.contains(&(k.as_str(), key.as_str())))
        }
        _ => !MINE.contains(&("", k.as_str())),
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
                e.message()
            ));
        }
    };
    if !has_other_settings(&table) {
        return Decision::Write(wanted);
    }
    let differ: Vec<String> = ours
        .keys()
        .into_iter()
        .filter(|(at, key, value)| lookup(&table, at, key).and_then(Value::as_str) != Some(value))
        .map(|(at, key, value)| {
            if at.is_empty() {
                format!("{key} should be {value:?}")
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
