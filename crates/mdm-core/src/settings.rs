//! The settings file: `KEY=VALUE` lines over a closed set of keys.
//!
//! Blank lines and lines starting with `#` are skipped. A key is an upper-case letter
//! followed by upper-case letters, digits and `_`; the value is the rest of the line,
//! with one pair of surrounding double quotes removed. A key outside the set, or one
//! given twice, is refused, so a typo is an error rather than a setting nobody reads.
//! The text comes from the caller: this crate opens no file.

use std::collections::BTreeMap;

/// The settings a file gives, by key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    values: BTreeMap<String, String>,
}

/// Why a settings file was refused. Line numbers count from 1.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum SettingsError {
    /// A line that is not `KEY=VALUE`.
    #[error("line {line}: not KEY=VALUE")]
    Malformed {
        /// The line number.
        line: usize,
    },
    /// A key outside the closed set.
    #[error("line {line}: unknown key {key}")]
    Unknown {
        /// The line number.
        line: usize,
        /// The key.
        key: String,
    },
    /// A key given a second time.
    #[error("line {line}: {key} is already set on line {first}")]
    Repeated {
        /// The line number of the second setting.
        line: usize,
        /// The key.
        key: String,
        /// The line number of the first.
        first: usize,
    },
    /// A key that is not set, or set empty.
    #[error("{0} is not set (settings file)")]
    Missing(String),
}

impl Settings {
    /// Parse `text`, accepting only the keys in `keys`.
    pub fn parse(text: &str, keys: &[&str]) -> Result<Settings, SettingsError> {
        let mut values = BTreeMap::new();
        let mut first_line = BTreeMap::new();
        for (i, raw) in text.lines().enumerate() {
            let line = i + 1;
            if raw.is_empty() || raw.starts_with('#') {
                continue;
            }
            let Some((key, value)) = raw.split_once('=').filter(|(k, _)| is_key(k)) else {
                return Err(SettingsError::Malformed { line });
            };
            if !keys.contains(&key) {
                return Err(SettingsError::Unknown {
                    line,
                    key: key.to_owned(),
                });
            }
            if let Some(&first) = first_line.get(key) {
                return Err(SettingsError::Repeated {
                    line,
                    key: key.to_owned(),
                    first,
                });
            }
            first_line.insert(key, line);
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(value);
            values.insert(key.to_owned(), value.to_owned());
        }
        Ok(Settings { values })
    }

    /// The value of `key`, if the file set it (possibly empty).
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    /// The value of `key`, refusing a key the file did not set or set empty.
    pub fn require(&self, key: &str) -> Result<&str, SettingsError> {
        match self.get(key) {
            Some(v) if !v.is_empty() => Ok(v),
            _ => Err(SettingsError::Missing(key.to_owned())),
        }
    }

    /// The value of `key`, or `default` when the file does not set it. A key set empty
    /// is refused, as by [`Settings::require`]: an empty value is not a way to ask for
    /// the default.
    pub fn get_or(&self, key: &str, default: &str) -> Result<String, SettingsError> {
        match self.get(key) {
            None => Ok(default.to_owned()),
            Some(_) => self.require(key).map(str::to_owned),
        }
    }
}

fn is_key(k: &str) -> bool {
    let mut chars = k.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}
