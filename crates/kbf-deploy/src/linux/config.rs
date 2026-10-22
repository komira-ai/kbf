//! `linux swap`'s settings: its closed key set, the defaults, and the check every value
//! passes.

use std::path::Path;

use crate::config::{Config, ConfigError};
use kbf_host::{HostPath, PathError};

/// The keys `swap` reads.
const SWAP_KEYS: &[&str] = &["KBF_SWAP_FILE", "KBF_SWAP_SIZE"];

/// The keys `templates/linux/swap.example` documents, and those of the retired unit
/// verbs' node settings file, which an existing file may still carry: ignored with a
/// warning ([`SwapConfig::retired`]). Any other key is refused.
pub const KEYS: &[&str] = &[
    "KBF_HOST_NAME",
    "KBF_LISTEN_ADDRESS",
    "KBF_NODE_ID",
    "KBF_REAPI_PORT",
    "KBF_WORKER_PORT",
    "KBF_API_PORT",
    "KBF_STORAGE_PORT",
    "KBF_RAM_GIB",
    "MINIO_RESERVATION_GIB",
    "KBF_SERVER_RESERVATION_GIB",
    "KBF_OS_HEADROOM_GIB",
    "KBF_DAEMON_MEMORY_MIN_MIB",
    "MINIO_UNIT",
    "MINIO_S3_ENDPOINT",
    "KBF_S3_BUCKET",
    "KBF_S3_PREFIX",
    "KBF_META_DIR",
    "KBF_HOT_DIR",
    "KBF_DATA_DIR",
    "KBF_IMAGES",
    "KBF_USER",
    "KBF_SERVER_USER",
    "KBF_ETC",
    "KBF_BIN_DIR",
    "KBF_SWAP_FILE",
    "KBF_SWAP_SIZE",
    "KBF_FRONTEND_NAME",
    "KBF_STORAGE_URL",
    "KBF_REAPI_CERT",
    "KBF_REAPI_KEY",
    "KBF_API_TOKEN_FILE",
    "KBF_STATE_STORE",
    "KBF_RESERVE_CPUS",
    "KBF_LEASE_PLACEMENT",
];

/// The settings, checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwapConfig {
    pub swap_file: HostPath,
    pub swap_size: String,
    /// Retired keys the file sets, ignored: the caller warns.
    pub retired: Vec<&'static str>,
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
    /// The warning for the retired keys the file sets, `None` when it sets none.
    #[must_use]
    pub fn retired_warning(&self) -> Option<String> {
        (!self.retired.is_empty()).then(|| {
            format!(
                "kbf-deploy: warning: {} no longer read; see docs/deploy/kbf-units.md",
                self.retired.join(", ")
            )
        })
    }

    /// Read and check the file at `path`.
    pub fn load(path: &Path) -> Result<SwapConfig, SwapConfigError> {
        SwapConfig::from_config(&Config::load(path, KEYS)?)
    }

    /// Check `text`, a config file's contents.
    pub fn parse(text: &str) -> Result<SwapConfig, SwapConfigError> {
        SwapConfig::from_config(&Config::parse(text, KEYS)?)
    }

    fn from_config(c: &Config) -> Result<SwapConfig, SwapConfigError> {
        for &key in SWAP_KEYS {
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
            retired: KEYS
                .iter()
                .copied()
                .filter(|k| !SWAP_KEYS.contains(k) && c.get(k).is_some())
                .collect(),
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
