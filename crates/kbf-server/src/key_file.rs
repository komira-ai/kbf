//! The S3 key pair in a file, for an `s3://` `--cold-store` and the pack store.

use std::path::Path;

use kbf_objstore::s3::{Credentials, S3Store};

use crate::Args;
use crate::config::ConfigError;
use crate::stores::S3Uri;

/// Why a key file is refused. Holds no byte of the file.
#[derive(Debug)]
#[allow(dead_code, reason = "red-first stub")]
pub(crate) struct KeyFileError(String);

impl std::fmt::Display for KeyFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The key pair in the file at `path`.
#[allow(dead_code, reason = "red-first stub")]
pub(crate) fn read_key_pair(_path: &Path) -> Result<Credentials, KeyFileError> {
    Err(KeyFileError("not implemented".to_owned()))
}

/// The cold store's key pair: the file's, else the environment's.
#[allow(dead_code, reason = "red-first stub")]
pub(crate) fn cold_keys(
    _file: Option<&Path>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Credentials, ConfigError> {
    let need = |name: &'static str| env(name).ok_or(ConfigError::MissingEnv(name));
    Ok(Credentials::new(
        need("AWS_ACCESS_KEY_ID")?,
        need("AWS_SECRET_ACCESS_KEY")?,
    ))
}

/// The S3 store of `uri` on [`cold_keys`].
pub(crate) fn cold_s3_store(
    uri: &S3Uri,
    file: Option<&Path>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<S3Store, ConfigError> {
    crate::s3_store(uri, |name| env(name).filter(|_| file.is_none()))
}

impl Args {
    /// Refuses `--cold-store-credentials-file` with a cold store that is not `s3://`.
    pub(crate) fn check_cold_key_file(&self) -> Result<(), ConfigError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
