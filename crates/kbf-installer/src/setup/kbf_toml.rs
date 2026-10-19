//! The `/etc/kbf/kbf.toml` that `setup` writes: the host's mode; for a node mode the frontend
//! URL and the enrollment token's path (the keys enrollment reads, `kbf-enroll`'s `KEYS`); for
//! a host that serves the operator API the path of its token (`api-token-file`, group `tls`) and
//! the state database (`state-store`, group `scheduler`); and the listener, certificate and
//! storage-client settings the named flags give ([`super::kbf_settings`]). Everything else in
//! the file is the mode's other flags, which the operator adds; `setup` never writes an update
//! or installer key (those are `installer.toml`).
//!
//! `setup` owns the keys it can write ([`owned`]). A file that exists is kept as it is when
//! every key the flags give already has the flag's value. Otherwise it is rewritten, whole and
//! atomically, with the flags' values in those keys and every other key as it was; the keys
//! that changed are named in the result (names only, never values). A key `setup` owns that the
//! mode takes no flag for (a frontend URL left over from another mode) is removed and named; a
//! key it owns that this run's flags leave out is left as it is. A rewrite does not keep the
//! comments of the old file. A file that is not TOML is not touched (exit 5).

use toml::{Table, Value};

use super::kbf_settings::{self, Setting};

/// What `setup` says goes in the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ours<'a> {
    pub mode: &'static str,
    pub frontend_url: Option<&'a str>,
    pub token_file: Option<&'a str>,
    pub api_token_file: Option<&'a str>,
    /// The state database of a frontend or all-in-one host: `[scheduler] state-store`.
    pub state_store: Option<&'a str>,
    /// The listener, certificate and storage-client settings the flags gave.
    pub extra: &'a [Setting],
    /// The `(table, key)` pairs of [`owned`] this mode takes a flag for; the other owned keys
    /// are removed from a file that has them.
    pub takes: Vec<(&'static str, &'static str)>,
}

/// Every key `setup` can write: `(table, key)`, the table empty for a top-level key.
pub fn owned() -> Vec<(&'static str, &'static str)> {
    let mut all = vec![
        ("", "mode"),
        ("frontend", "url"),
        ("enroll", "token_file"),
        ("tls", "api-token-file"),
        ("scheduler", "state-store"),
    ];
    all.extend(kbf_settings::keys());
    all
}

impl Ours<'_> {
    /// The keys the flags give, as `(table, key, value)`.
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

    /// The `(table, key)` pairs this run owns a value for or takes a flag for.
    fn takes(&self) -> Vec<(&'static str, &'static str)> {
        let mut out: Vec<_> = self.keys().into_iter().map(|(t, k, _)| (t, k)).collect();
        out.extend(self.takes.iter().copied());
        out
    }

    /// The canonical file for a host with no `kbf.toml`.
    pub fn render(&self) -> String {
        let mut table = Table::new();
        for (at, key, value) in self.keys() {
            set(&mut table, at, key, &value);
        }
        render_table(&table)
    }
}

/// What to do about the file that is there.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// Make the file this text. `changed` names the keys of a file that was there which
    /// this differs in (`[table] key`, or `key` at the top level); empty when the text is
    /// the file's own or the file is new.
    Write { text: String, changed: Vec<String> },
    /// Only a person can go on; the reason, one line, with no value in it.
    Refuse(String),
}

fn lookup<'t>(table: &'t Table, at: &str, key: &str) -> Option<&'t Value> {
    if at.is_empty() {
        table.get(key)
    } else {
        table.get(at)?.as_table()?.get(key)
    }
}

/// `table[at][key] = value`, making `at` a table if it is not one.
fn set(table: &mut Table, at: &str, key: &str, value: &str) {
    let value = Value::String(value.to_owned());
    if at.is_empty() {
        table.insert(key.to_owned(), value);
        return;
    }
    let slot = table
        .entry(at.to_owned())
        .or_insert_with(|| Value::Table(Table::new()));
    if !slot.is_table() {
        *slot = Value::Table(Table::new());
    }
    if let Value::Table(inner) = slot {
        inner.insert(key.to_owned(), value);
    }
}

/// Remove `table[at][key]`, and `at` if that leaves it empty.
fn remove(table: &mut Table, at: &str, key: &str) {
    if at.is_empty() {
        table.remove(key);
        return;
    }
    if let Some(Value::Table(inner)) = table.get_mut(at) {
        inner.remove(key);
        if inner.is_empty() {
            table.remove(at);
        }
    }
}

fn name(at: &str, key: &str) -> String {
    if at.is_empty() {
        key.to_owned()
    } else {
        format!("[{at}] {key}")
    }
}

/// Decide what `setup` does with the existing file `current` (`None`: there is none).
pub fn decide(current: Option<&str>, ours: &Ours<'_>) -> Decision {
    let Some(text) = current else {
        return Decision::Write {
            text: ours.render(),
            changed: Vec::new(),
        };
    };
    let mut table: Table = match text.parse() {
        Ok(t) => t,
        Err(e) => {
            return Decision::Refuse(format!(
                "{} is not valid TOML ({}): fix it or remove it, then run setup again",
                super::paths::KBF_TOML,
                crate::installer_toml::redact(e.message())
            ));
        }
    };
    let mut changed = Vec::new();
    for (at, key, value) in ours.keys() {
        if lookup(&table, at, key).and_then(Value::as_str) != Some(value.as_str()) {
            set(&mut table, at, key, &value);
            changed.push(name(at, key));
        }
    }
    let takes = ours.takes();
    for (at, key) in owned() {
        if !takes.contains(&(at, key)) && lookup(&table, at, key).is_some() {
            remove(&mut table, at, key);
            changed.push(name(at, key));
        }
    }
    if changed.is_empty() {
        return Decision::Write {
            text: text.to_owned(),
            changed,
        };
    }
    Decision::Write {
        text: render_table(&table),
        changed,
    }
}

const HEADER: &str = "# Written by `kbf-installer setup`. The mode's other settings are added below the\n\
                      # keys it writes; run setup again to change these.\n";

/// The file for `table`: the header, the top-level keys with `mode` first, then each table.
fn render_table(table: &Table) -> String {
    let mut out = String::from(HEADER);
    if let Some(mode) = table.get("mode") {
        out.push_str(&format!("mode = {}\n", value(mode)));
    }
    for (key, v) in table {
        if key != "mode" && !v.is_table() {
            out.push_str(&format!("{} = {}\n", bare(key), value(v)));
        }
    }
    for (key, v) in table {
        if let Value::Table(inner) = v {
            tables(&mut out, &bare(key), inner);
        }
    }
    out
}

/// `[path]` and its keys, then its sub-tables as `[path.sub]`.
fn tables(out: &mut String, path: &str, table: &Table) {
    out.push_str(&format!("\n[{path}]\n"));
    for (key, v) in table {
        if !v.is_table() {
            out.push_str(&format!("{} = {}\n", bare(key), value(v)));
        }
    }
    for (key, v) in table {
        if let Value::Table(inner) = v {
            tables(out, &format!("{path}.{}", bare(key)), inner);
        }
    }
}

/// A key as TOML writes it: bare when it can be, else quoted.
fn bare(key: &str) -> String {
    if !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        key.to_owned()
    } else {
        string(key)
    }
}

fn string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A value on one line (a table inside an array is an inline table).
fn value(v: &Value) -> String {
    match v {
        Value::String(s) => string(s),
        Value::Integer(i) => i.to_string(),
        Value::Float(f) if f.is_nan() => "nan".to_owned(),
        Value::Float(f) if f.is_infinite() => if *f > 0.0 { "inf" } else { "-inf" }.to_owned(),
        Value::Float(f) => {
            let text = f.to_string();
            if text.contains('.') || text.contains('e') {
                text
            } else {
                format!("{text}.0")
            }
        }
        Value::Boolean(b) => b.to_string(),
        Value::Datetime(d) => d.to_string(),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(value).collect::<Vec<_>>().join(", ")
        ),
        Value::Table(t) => format!(
            "{{ {} }}",
            t.iter()
                .map(|(k, v)| format!("{} = {}", bare(k), value(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}
