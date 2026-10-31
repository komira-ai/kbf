//! The `--params <file>` table.
//!
//! The declaration's `[hook.params]` table is opaque to the updater: it checks that every value
//! is a scalar, writes the table to a file and passes the file's path. The file is TOML, a flat
//! table of scalars. The updater adds one reserved key, [`PROMOTION_CEILING_KEY`], the one
//! ceiling on a pending promotion, which the hook must not itself fail for.
//!
//! The reserved key contains a dot, so in the file it is a quoted key:
//! `"updater.promotion_ceiling_secs" = 3600`. Written bare, TOML makes it a table `updater`, which
//! is not a scalar and is refused.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use serde::Serialize;

/// The reserved key the updater sets from the declaration's `promotion_ceiling_secs`.
pub const PROMOTION_CEILING_KEY: &str = "updater.promotion_ceiling_secs";

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
}

/// A flat table of scalars.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Params(BTreeMap<String, Scalar>);

impl Params {
    /// Reads a params file.
    pub fn read(path: &Path) -> Result<Params, ParamsError> {
        Params::parse(&std::fs::read_to_string(path)?)
    }

    /// Reads the text of a params file.
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
}
