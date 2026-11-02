//! The S3 key pair in a file: `--cold-store-credentials-file` for an `s3://` `--cold-store`,
//! and `--pack-store-credentials-file` for the pack store, one reader for both.
//!
//! The file is JSON, `{"access_key_id": "...", "secret_access_key": "..."}`. It must be a
//! regular file the server's user owns, mode 0600 or 0400, and not a symbolic link (the
//! rules of [`crate::token::read_owner_only_nofollow`], the reader of the other secret files
//! of the server). No message made here holds a byte of the file: a parse error is replaced
//! by a fixed text, since the text serde gives can repeat a value.
//!
//! The cold store may still take its key pair from `AWS_ACCESS_KEY_ID` and
//! `AWS_SECRET_ACCESS_KEY`, for one more release: the start logs one warning, which names
//! the variables and no value ([`cold_keys`]).

use std::path::Path;

use kbf_objstore::s3::{Credentials, S3Store};
use serde::Deserialize;
use zeroize::Zeroizing;

use crate::Args;
use crate::config::{ConfigError, s3_store_keyed};
use crate::stores::{S3Uri, StoreUri};
use crate::token::read_owner_only_nofollow;

/// The most bytes a key file may hold: far more than a key pair takes.
const MAX_KEY_FILE_BYTES: usize = 4096;

/// What a file that is not the key pair is told.
const NOT_KEYS: &str = "not JSON with a non-empty access_key_id and secret_access_key";

/// Why a key file is refused. Holds no byte of the file.
#[derive(Debug)]
pub(crate) struct KeyFileError(String);

impl std::fmt::Display for KeyFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a key file holds.
#[derive(Deserialize)]
struct Keys {
    access_key_id: String,
    secret_access_key: String,
}

/// The key pair in the file at `path`.
///
/// # Errors
/// The file cannot be read, is a symbolic link or not a regular file, is not owned by this
/// process's user, has a mode other than 0600 or 0400, is longer than
/// [`MAX_KEY_FILE_BYTES`], or is not JSON holding a non-empty `access_key_id` and
/// `secret_access_key`.
pub(crate) fn read_key_pair(path: &Path) -> Result<Credentials, KeyFileError> {
    let bytes = read_owner_only_nofollow(path, MAX_KEY_FILE_BYTES)
        .map_err(|error| KeyFileError(error.reason()))?;
    let bytes = Zeroizing::new(bytes);
    let not_keys = || KeyFileError(NOT_KEYS.to_owned());
    if bytes.len() > MAX_KEY_FILE_BYTES {
        return Err(not_keys());
    }
    let keys = serde_json::from_slice::<Keys>(&bytes).map_err(|_| not_keys())?;
    if keys.access_key_id.is_empty() || keys.secret_access_key.is_empty() {
        return Err(not_keys());
    }
    Ok(Credentials::new(keys.access_key_id, keys.secret_access_key))
}

/// The cold store's key pair: the pair in `file` when `--cold-store-credentials-file` is given
/// (the environment is not read), else the one `env` gives for `AWS_ACCESS_KEY_ID` and
/// `AWS_SECRET_ACCESS_KEY`, with a warning that the variables are deprecated.
///
/// # Errors
/// [`ConfigError::ColdKeyFile`] when the file is refused; [`ConfigError::MissingEnv`] when
/// there is no file and a variable is missing.
pub(crate) fn cold_keys(
    file: Option<&Path>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Credentials, ConfigError> {
    if let Some(path) = file {
        return read_key_pair(path).map_err(|error| ConfigError::ColdKeyFile {
            path: path.to_owned(),
            why: error.to_string(),
        });
    }
    let need = |name: &'static str| env(name).ok_or(ConfigError::MissingEnv(name));
    let credentials = Credentials::new(need("AWS_ACCESS_KEY_ID")?, need("AWS_SECRET_ACCESS_KEY")?);
    tracing::warn!(
        "--cold-store: the key pair from AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY is \
         deprecated and a later release refuses it: put it in a file only this user can read \
         and pass --cold-store-credentials-file"
    );
    Ok(credentials)
}

/// The S3 store of `uri` signed with [`cold_keys`].
///
/// # Errors
/// As [`cold_keys`], or the store's configuration is refused.
pub(crate) fn cold_s3_store(
    uri: &S3Uri,
    file: Option<&Path>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<S3Store, ConfigError> {
    s3_store_keyed(uri, cold_keys(file, env)?)
}

impl Args {
    /// Refuses `--cold-store-credentials-file` with a cold store that is not `s3://`: a
    /// directory has no keys, so the flag would do nothing, and a `registered` store gets its
    /// keys from the registration, so the file would be a second place they come from.
    ///
    /// # Errors
    /// [`ConfigError::ColdKeyFileOnFileStore`], [`ConfigError::ColdKeyFileOnRegistered`].
    pub(crate) fn check_cold_key_file(&self) -> Result<(), ConfigError> {
        match (&self.cold_store_credentials_file, &self.cold_store) {
            (Some(_), Some(StoreUri::File(_))) => Err(ConfigError::ColdKeyFileOnFileStore),
            (Some(_), Some(StoreUri::Registered)) => Err(ConfigError::ColdKeyFileOnRegistered),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests;
