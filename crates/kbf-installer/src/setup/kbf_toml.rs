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

/// Insert `key = "value"` into the text of the table `at` (the top level when empty), after
/// the table's last line, or at the end of the file in a new table when it has none.
fn insert_key(text: &str, at: &str, key: &str, value: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let line = format!("{key} = \"{value}\"");
    let is_header = |l: &String| l.trim_start().starts_with('[');
    let start = if at.is_empty() {
        // The top level is above the first table.
        Some(usize::MAX)
    } else {
        lines.iter().position(|l| l.trim() == format!("[{at}]"))
    };
    match start {
        Some(first) => {
            let from = if first == usize::MAX { 0 } else { first + 1 };
            let mut end = lines[from..]
                .iter()
                .position(is_header)
                .map_or(lines.len(), |i| from + i);
            while end > from && lines[end - 1].trim().is_empty() {
                end -= 1;
            }
            lines.insert(end, line);
        }
        None => {
            lines.push(String::new());
            lines.push(format!("[{at}]"));
            lines.push(line);
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// What `setup` does with the `kbf.toml` of a host the Mac kit installed and `migrate` adopted
/// (the migration's journal says so, not the file): every key the kit wrote stays as it is,
/// byte for byte, and the keys setup needs that are missing are added. A key that is there
/// with another value than the one setup needs is not rewritten: it is refused, naming the
/// key. The installer's directories come in `ours.update` (`[daemon] update-dir` and
/// `orders-dir`, so the daemon spools orders where the installer reads them).
pub fn decide_adopted(text: &str, ours: &Ours<'_>, path: &str) -> Decision {
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
    let wanted = ours.keys();
    let (mut differ, mut missing) = (Vec::new(), Vec::new());
    for (at, key, value) in &wanted {
        match lookup(&table, at, key) {
            None => missing.push((*at, *key, value)),
            Some(have) if have.as_str() == Some(value) => {}
            Some(_) => differ.push(if at.is_empty() {
                (*key).to_owned()
            } else {
                format!("[{at}] {key}")
            }),
        }
    }
    if !differ.is_empty() {
        return Decision::Refuse(format!(
            "{path} is the Mac kit's file, adopted by migrate, and these keys differ from what setup needs ({}): setup never rewrites a key the kit wrote; edit them by hand, or change the flags",
            differ.join(", ")
        ));
    }
    if missing.is_empty() {
        return Decision::Keep;
    }
    let mut out = text.to_owned();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    for (at, key, value) in &missing {
        out = insert_key(&out, at, key, value);
    }
    // The edit is checked by what it parses to: the kit's keys are all as they were and each
    // added key says what was asked. A file the text edit cannot add to (a table written as
    // an inline table, say) is refused, not guessed at.
    let after: Result<Table, _> = out.parse();
    let kept = after.as_ref().is_ok_and(|after| {
        missing
            .iter()
            .all(|(at, key, value)| lookup(after, at, key).and_then(Value::as_str) == Some(value))
            && table.iter().all(|(k, v)| match v {
                Value::Table(inner) => inner
                    .iter()
                    .all(|(ik, iv)| lookup(after, k, ik) == Some(iv)),
                _ => after.get(k) == Some(v),
            })
    });
    if kept {
        Decision::Write(out)
    } else {
        let names: Vec<String> = missing
            .iter()
            .map(|(at, key, _)| format!("[{at}] {key}"))
            .collect();
        Decision::Refuse(format!(
            "{path} is the Mac kit's file and setup cannot add {} to it without changing a key the kit wrote: add the key by hand, then run setup again",
            names.join(", ")
        ))
    }
}
