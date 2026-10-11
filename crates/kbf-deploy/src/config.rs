//! A kit's `--config FILE`: `KEY=VALUE` lines over a closed set of keys.
//!
//! Blank lines and lines starting with `#` are skipped. A key is an upper-case letter
//! followed by upper-case letters, digits and `_`; the value is the rest of the line,
//! with one pair of surrounding double quotes removed. A key outside the kit's set, or
//! one given twice, is refused, so a typo is an error rather than a setting nobody
//! reads.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

/// The settings a config file gives, by key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    values: BTreeMap<String, String>,
}

/// Why a config file was refused. Line numbers count from 1.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("line {line}: not KEY=VALUE")]
    Malformed { line: usize },
    #[error("line {line}: unknown key {key}")]
    Unknown { line: usize, key: String },
    #[error("line {line}: {key} is already set on line {first}")]
    Repeated {
        line: usize,
        key: String,
        first: usize,
    },
    #[error("{0} is not set (config file)")]
    Missing(String),
}

impl Config {
    /// Parse `text`, accepting only the keys in `keys`.
    pub fn parse(text: &str, keys: &[&str]) -> Result<Config, ConfigError> {
        let mut values = BTreeMap::new();
        let mut first_line = BTreeMap::new();
        for (i, raw) in text.lines().enumerate() {
            let line = i + 1;
            if raw.is_empty() || raw.starts_with('#') {
                continue;
            }
            let Some((key, value)) = raw.split_once('=').filter(|(k, _)| is_key(k)) else {
                return Err(ConfigError::Malformed { line });
            };
            if !keys.contains(&key) {
                return Err(ConfigError::Unknown {
                    line,
                    key: key.to_owned(),
                });
            }
            if let Some(&first) = first_line.get(key) {
                return Err(ConfigError::Repeated {
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
        Ok(Config { values })
    }

    /// Read and parse the file at `path`.
    pub fn load(path: &Path, keys: &[&str]) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        Config::parse(&text, keys)
    }

    /// The value of `key`, if the file set it.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    /// The value of `key`, refusing a key the file did not set or set empty.
    pub fn require(&self, key: &str) -> Result<&str, ConfigError> {
        match self.get(key) {
            Some(v) if !v.is_empty() => Ok(v),
            _ => Err(ConfigError::Missing(key.to_owned())),
        }
    }
}

fn is_key(k: &str) -> bool {
    let mut chars = k.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYS: &[&str] = &[
        "KBF_HOST_NAME",
        "KBF_RAM_GIB",
        "KBF_IMAGES",
        "MINIO_S3_ENDPOINT",
    ];

    fn refusal(text: &str) -> String {
        Config::parse(text, KEYS)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default()
    }

    /// Catches a parser that takes a typo'd key (the setting would be silently
    /// ignored), lets a second line override the first, or reads a line that is not
    /// `KEY=VALUE`.
    #[test]
    fn unknown_repeated_and_malformed_lines_are_refused() {
        let cases = [
            ("KBF_HOST_NAMES=a\n", "line 1: unknown key KBF_HOST_NAMES"),
            (
                "KBF_RAM_GIB=64\n# c\nKBF_RAM_GIB=128\n",
                "line 3: KBF_RAM_GIB is already set on line 1",
            ),
            ("KBF_RAM_GIB 64\n", "line 1: not KEY=VALUE"),
            ("\nkbf_ram_gib=64\n", "line 2: not KEY=VALUE"),
            ("1KBF=1\n", "line 1: not KEY=VALUE"),
            ("KBF-RAM=1\n", "line 1: not KEY=VALUE"),
            ("=1\n", "line 1: not KEY=VALUE"),
            (" KBF_RAM_GIB=64\n", "line 1: not KEY=VALUE"),
        ];
        for (text, want) in cases {
            assert_eq!(refusal(text), want, "{text:?}");
        }
    }

    /// Catches losing a value's `=`, spaces or empty value, and quotes stripped from one
    /// side only.
    #[test]
    fn values_are_the_rest_of_the_line_less_one_pair_of_quotes() {
        let text = "# kbf-linux\n\nKBF_HOST_NAME=\"node a\"\nKBF_RAM_GIB=a=b\nKBF_IMAGES=\nMINIO_S3_ENDPOINT=http://127.0.0.1:9000\n";
        let c = Config::parse(text, KEYS).expect("valid");
        assert_eq!(c.get("KBF_HOST_NAME"), Some("node a"));
        assert_eq!(c.get("KBF_RAM_GIB"), Some("a=b"));
        assert_eq!(c.get("KBF_IMAGES"), Some(""));
        assert_eq!(c.get("MINIO_S3_ENDPOINT"), Some("http://127.0.0.1:9000"));
        let half = Config::parse("KBF_HOST_NAME=\"a\nKBF_RAM_GIB=b\"\n", KEYS).expect("valid");
        assert_eq!(half.get("KBF_HOST_NAME"), Some("\"a"));
        assert_eq!(half.get("KBF_RAM_GIB"), Some("b\""));
    }

    /// Catches `require` taking an empty value or an unset key as set.
    #[test]
    fn require_refuses_an_unset_or_empty_key() {
        let c = Config::parse("KBF_RAM_GIB=64\nKBF_IMAGES=\n", KEYS).expect("valid");
        assert_eq!(c.require("KBF_RAM_GIB").ok(), Some("64"));
        for key in ["KBF_IMAGES", "KBF_HOST_NAME"] {
            let err = c.require(key).err().map(|e| e.to_string());
            assert_eq!(err, Some(format!("{key} is not set (config file)")));
        }
    }

    /// Catches a missing file being read as an empty config (every key unset).
    #[test]
    fn load_reads_the_file_and_refuses_a_missing_one() {
        let dir = crate::testkit::scratch("config_load");
        let file = dir.join("config.local");
        std::fs::write(&file, "KBF_RAM_GIB=64\n").expect("write");
        let c = Config::load(&file, KEYS).expect("valid");
        assert_eq!(c.get("KBF_RAM_GIB"), Some("64"));
        let gone = dir.join("missing");
        let err = Config::load(&gone, KEYS).err().map(|e| e.to_string());
        assert!(err.is_some_and(|e| e.starts_with("cannot read ")));
    }
}
