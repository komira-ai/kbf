//! The `--params <file>` table.
//!
//! The declaration's `[hook.params]` table is opaque to the updater: it checks that every value
//! is a scalar, writes the table to a file and passes the file's path. The file is TOML, a flat
//! table of scalars. The updater adds one reserved key, [`PROMOTION_CEILING_KEY`], the one
//! ceiling on a pending promotion, which the hook must not itself fail for.
//!
//! The updater writes more reserved keys, all in the namespace `updater.`, which a hook only
//! reads: [`STAGED_RELEASE_KEY`], [`PREVIOUS_RELEASE_KEY`] (absent on a first install),
//! [`TARGET_VERSION_KEY`] and [`PREVIOUS_VERSION_KEY`] (absent on a first install). The staged
//! release equals the argument of `prepare`. A declaration's own `[hook.params]` may not set any
//! `updater.*` key: [`Params::parse_declared`] refuses it. **A hook still validates every one of
//! these values against its own record directory and its declaration**: they come from a file,
//! and a value read from a file is untrusted input.
//!
//! The reserved key contains a dot, so in the file it is a quoted key:
//! `"updater.promotion_ceiling_secs" = 3600`. Written bare, TOML makes it a table `updater`, which
//! is not a scalar and is refused.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

/// The reserved key the updater sets from the declaration's `promotion_ceiling_secs`.
pub const PROMOTION_CEILING_KEY: &str = "updater.promotion_ceiling_secs";

/// The absolute path of the verified staged release tree.
pub const STAGED_RELEASE_KEY: &str = "updater.staged_release";
/// The absolute path of the previous release; absent on a first install.
pub const PREVIOUS_RELEASE_KEY: &str = "updater.previous_release";
/// The version being installed.
pub const TARGET_VERSION_KEY: &str = "updater.target_version";
/// The version it replaces; absent on a first install.
pub const PREVIOUS_VERSION_KEY: &str = "updater.previous_version";
/// The reserved keys whose value is text.
const TEXT_KEYS: [&str; 4] = [
    STAGED_RELEASE_KEY,
    PREVIOUS_RELEASE_KEY,
    TARGET_VERSION_KEY,
    PREVIOUS_VERSION_KEY,
];
/// The namespace of the keys only the updater writes.
const RESERVED_PREFIX: &str = "updater.";

/// One value of the table.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Scalar {
    Bool(bool),
    Integer(i64),
    Float(f64),
    Text(String),
}

/// Why a params file was refused.
#[derive(Debug, thiserror::Error)]
pub enum ParamsError {
    #[error("cannot read the params file: {0}")]
    Read(#[from] io::Error),
    #[error("the params file is not a TOML table: {0}")]
    Syntax(String),
    #[error("params key {0:?} is not a scalar (a dotted key must be quoted)")]
    NotScalar(String),
    #[error("{PROMOTION_CEILING_KEY} must be a whole number of seconds, not negative")]
    BadCeiling,
    #[error("reserved key {0:?} must be text")]
    BadReserved(String),
    #[error("params key {0:?} is in the namespace `updater.`, which only the updater writes")]
    ReservedKey(String),
}

/// A flat table of scalars.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Params(BTreeMap<String, Scalar>);

impl Params {
    /// Reads a params file.
    pub fn read(path: &Path) -> Result<Params, ParamsError> {
        Params::parse(&std::fs::read_to_string(path)?)
    }

    /// Reads a declaration's own `[hook.params]` table, which may not set any `updater.*` key.
    pub fn parse_declared(text: &str) -> Result<Params, ParamsError> {
        let params = Params::parse(text)?;
        match params.0.keys().find(|key| key.starts_with(RESERVED_PREFIX)) {
            Some(key) => Err(ParamsError::ReservedKey(key.clone())),
            None => Ok(params),
        }
    }

    /// Reads the text of a params file, as the updater wrote it.
    pub fn parse(text: &str) -> Result<Params, ParamsError> {
        let table: toml::Table = text
            .parse()
            .map_err(|e: toml::de::Error| ParamsError::Syntax(e.message().to_owned()))?;
        let mut values = BTreeMap::new();
        for (key, value) in table {
            let scalar = match value {
                toml::Value::Boolean(b) => Scalar::Bool(b),
                toml::Value::Integer(n) => Scalar::Integer(n),
                toml::Value::Float(f) => Scalar::Float(f),
                toml::Value::String(s) => Scalar::Text(s),
                _ => return Err(ParamsError::NotScalar(key)),
            };
            values.insert(key, scalar);
        }
        let params = Params(values);
        if params.0.contains_key(PROMOTION_CEILING_KEY) && params.promotion_ceiling_secs().is_none()
        {
            return Err(ParamsError::BadCeiling);
        }
        if let Some(key) = TEXT_KEYS.into_iter().find(|key| {
            params
                .0
                .get(*key)
                .is_some_and(|v| !matches!(v, Scalar::Text(_)))
        }) {
            return Err(ParamsError::BadReserved(key.to_owned()));
        }
        Ok(params)
    }

    /// The table as the file the updater writes.
    pub fn to_toml(&self) -> String {
        // A flat map of scalars always serializes; an empty table is the empty file.
        toml::to_string(&self.0).unwrap_or_default()
    }

    /// Sets a value.
    pub fn insert(&mut self, key: &str, value: Scalar) {
        self.0.insert(key.to_owned(), value);
    }

    /// Sets the reserved ceiling.
    pub fn with_promotion_ceiling_secs(mut self, secs: u64) -> Params {
        self.insert(
            PROMOTION_CEILING_KEY,
            Scalar::Integer(i64::try_from(secs).unwrap_or(i64::MAX)),
        );
        self
    }

    pub fn get(&self, key: &str) -> Option<&Scalar> {
        self.0.get(key)
    }

    /// A whole, non-negative number.
    pub fn get_u64(&self, key: &str) -> Option<u64> {
        match self.get(key) {
            Some(Scalar::Integer(n)) => u64::try_from(*n).ok(),
            _ => None,
        }
    }

    pub fn get_bool(&self, key: &str) -> Option<bool> {
        match self.get(key) {
            Some(Scalar::Bool(b)) => Some(*b),
            _ => None,
        }
    }

    /// The reserved ceiling, when the updater set it.
    pub fn promotion_ceiling_secs(&self) -> Option<u64> {
        self.get_u64(PROMOTION_CEILING_KEY)
    }

    fn text(&self, key: &str) -> Option<&str> {
        match self.get(key) {
            Some(Scalar::Text(text)) => Some(text),
            _ => None,
        }
    }

    /// The verified staged release tree: the argument of `prepare`.
    pub fn staged_release(&self) -> Option<PathBuf> {
        self.text(STAGED_RELEASE_KEY).map(PathBuf::from)
    }

    /// The previous release; `None` on a first install.
    pub fn previous_release(&self) -> Option<PathBuf> {
        self.text(PREVIOUS_RELEASE_KEY).map(PathBuf::from)
    }

    /// The version being installed.
    pub fn target_version(&self) -> Option<&str> {
        self.text(TARGET_VERSION_KEY)
    }

    /// The version it replaces; `None` on a first install.
    pub fn previous_version(&self) -> Option<&str> {
        self.text(PREVIOUS_VERSION_KEY)
    }
}
