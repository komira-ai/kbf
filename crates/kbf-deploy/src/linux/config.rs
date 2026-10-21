//! `linux swap`'s settings: its closed key set, the defaults, and the check every value
//! passes.

use std::path::Path;

use crate::config::{Config, ConfigError};
use kbf_host::{HostPath, PathError};

/// The keys `templates/linux/swap.example` documents; any other is refused.
pub const KEYS: &[&str] = &["KBF_SWAP_FILE", "KBF_SWAP_SIZE"];

/// The settings, checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwapConfig {
    pub swap_file: HostPath,
    pub swap_size: String,
}

/// Why the settings were refused.
#[derive(Debug, thiserror::Error)]
pub enum SwapConfigError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("{key} holds a character the swap step cannot carry: {value}")]
    Chars { key: &'static str, value: String },
    #[error("{key}: {source}")]
    Path {
        key: &'static str,
        source: PathError,
    },
}

impl SwapConfig {
    /// Read and check the file at `path`.
    pub fn load(path: &Path) -> Result<SwapConfig, SwapConfigError> {
        SwapConfig::from_config(&Config::load(path, KEYS)?)
    }

    /// Check `text`, a config file's contents.
    pub fn parse(text: &str) -> Result<SwapConfig, SwapConfigError> {
        SwapConfig::from_config(&Config::parse(text, KEYS)?)
    }

    fn from_config(c: &Config) -> Result<SwapConfig, SwapConfigError> {
        for &key in KEYS {
            if let Some(value) = c.get(key) {
                plain(key, value)?;
            }
        }
        // An unset or empty key takes its default.
        let or = |key: &str, default: &str| -> String {
            c.get(key)
                .filter(|v| !v.is_empty())
                .unwrap_or(default)
                .to_owned()
        };
        Ok(SwapConfig {
            swap_file: HostPath::new(&or("KBF_SWAP_FILE", "/swapfile")).map_err(|source| {
                SwapConfigError::Path {
                    key: "KBF_SWAP_FILE",
                    source,
                }
            })?,
            swap_size: or("KBF_SWAP_SIZE", "128G"),
        })
    }
}

/// Refuse a value with a character outside letters, digits, spaces and `. _ : / = , + @ -`.
fn plain(key: &'static str, value: &str) -> Result<(), SwapConfigError> {
    let ok = value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || " ._:/=,+@-".contains(c));
    if ok {
        Ok(())
    } else {
        Err(SwapConfigError::Chars {
            key,
            value: value.to_owned(),
        })
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
