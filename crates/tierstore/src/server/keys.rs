//! The access keys of the S3 front: what a key may do, which key a request names, and the
//! files the keys are read from.
//!
//! A server has one full key, which may do everything the front serves
//! ([`ServerConfig::access_key_id`]), and any number of read-only keys
//! ([`ServerConfig::read_only_keys`]), which may do GetObject, HeadObject, ListObjectsV2 and
//! HeadBucket and nothing else: any other call signed by one is answered 403 `AccessDenied`
//! before its body is read, and changes nothing. A request names its key by the id in its
//! credential, so the ids of the keys of one server differ ([`ensure_distinct`], which the
//! binary applies at start).
//!
//! A credentials file (the file of `--access-key-file`, `--read-only-key-file`,
//! `--hot-credentials-file` or `--cold-credentials-file`) holds one key either as JSON,
//! `{"access_key_id": "...", "secret_access_key": "..."}`, which is what the pack flags of kbf
//! read (`access_key_id` and `secret_access_key`; tierstore also refuses any other field and an
//! empty value, and takes the values as they are, without trimming), or as two lines: the id on
//! the first line and the secret on the second. It is JSON when its first byte that is not white
//! space is `{`. Only its owner may be able to read it (no group or other permission bit), and no
//! error names the secret.

use std::collections::BTreeSet;
use std::fmt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use hyper::Method;
use serde::Deserialize;

use super::ServerConfig;
use super::auth::{READ_ONLY, Refusal};
use super::presign::PUT_ONLY;

/// The flag whose files hold the read-only keys.
const READ_ONLY_FLAG: &str = "--read-only-key-file";

/// What a credentials file in the two-line form is told when it is not one.
const LINES_FORM: &str = "the id is on the first line and the secret on the second";

/// What a credentials file in the JSON form is told when it is not one.
const JSON_FORM: &str =
    "not JSON of exactly access_key_id and secret_access_key, both non-empty strings";

/// An access key: the id a request names, and the secret that signs it.
#[derive(Clone, PartialEq, Eq)]
pub struct AccessKey {
    /// The access key id.
    pub id: String,
    /// The secret access key.
    pub secret: String,
}

impl AccessKey {
    /// The key with this id and this secret.
    #[must_use]
    pub fn new(id: &str, secret: &str) -> Self {
        Self {
            id: id.to_owned(),
            secret: secret.to_owned(),
        }
    }
}

impl fmt::Debug for AccessKey {
    /// Shows the id and never the secret.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccessKey")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// What a key may do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    /// Everything the front serves.
    Full,
    /// GetObject, HeadObject, ListObjectsV2 and HeadBucket.
    ReadOnly,
    /// A presigning key: one presigned `PUT`, as far as its grant goes (see `presign`).
    PresignPut,
}

impl Role {
    /// Whether a call with this method is one the role does. The reads are the `GET` and
    /// `HEAD` calls; a `PUT` (a put, a part, CreateBucket), a `DELETE` (a delete, an abort), a
    /// `POST` (the start and the completion of an upload) and every other method change
    /// something or are not served.
    ///
    /// # Errors
    /// [`READ_ONLY`], for a read-only key and a call that is not a read.
    pub(crate) fn permit(self, method: &Method) -> Result<(), Refusal> {
        let reads = matches!(*method, Method::GET | Method::HEAD);
        match (self, reads) {
            (Self::ReadOnly, false) => Err(READ_ONLY),
            (Self::PresignPut, _) if *method != Method::PUT => Err(PUT_ONLY),
            _ => Ok(()),
        }
    }
}

impl ServerConfig {
    /// The secret and the role of the key that has this id, if the server has one: the full key
    /// if the id is its, else the read-only key with it.
    pub(super) fn key(&self, id: &str) -> Option<(&str, Role)> {
        if id == self.access_key_id {
            return Some((self.secret_access_key.as_str(), Role::Full));
        }
        let found = self.read_only_keys.iter().find(|key| key.id == id);
        found.map(|key| (key.secret.as_str(), Role::ReadOnly))
    }
}

/// Refuses a server whose keys do not all have an id of their own: a request names its key by
/// id alone, so the second key with an id could never be told from the first.
///
/// # Errors
/// What is wrong, naming the id and the flags (never a secret).
pub(super) fn ensure_distinct(full: &str, read_only: &[AccessKey]) -> Result<(), String> {
    let mut seen = BTreeSet::from([full]);
    for key in read_only {
        if !seen.insert(key.id.as_str()) {
            let (id, flags) = (&key.id, "--access-key-file and --read-only-key-file");
            let why = format!("the access key id {id:?} is used twice among {flags}");
            return Err(why);
        }
    }
    Ok(())
}

/// The keys in the files of the flag `--read-only-key-file`, in the order of the flags.
///
/// # Errors
/// The first file that cannot be used, as [`read_credentials`] says.
pub(super) fn read_only_keys(paths: &[PathBuf]) -> Result<Vec<AccessKey>, String> {
    let read = |path: &PathBuf| read_credentials(READ_ONLY_FLAG, path);
    let pairs = paths.iter().map(read);
    pairs
        .map(|pair| pair.map(|(id, secret)| AccessKey { id, secret }))
        .collect()
}

/// What the JSON form of a credentials file holds.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonKey {
    access_key_id: String,
    secret_access_key: String,
}

/// The key in the JSON form: both fields, not empty, and no other field.
fn from_json(text: &str) -> Result<(String, String), &'static str> {
    let parsed = serde_json::from_str::<JsonKey>(text);
    // The error of the parser is dropped: for a value of the wrong type it quotes the value.
    let key = parsed.map_err(|_| JSON_FORM)?;
    let empty = [
        key.access_key_id.is_empty(),
        key.secret_access_key.is_empty(),
    ];
    if empty.contains(&true) {
        return Err(JSON_FORM);
    }
    Ok((key.access_key_id, key.secret_access_key))
}

/// The key in the two-line form: the id on the first line and the secret on the second, each
/// trimmed and not empty.
fn from_lines(text: &str) -> Result<(String, String), &'static str> {
    let mut lines = text.lines().map(str::trim);
    let id = lines.next().filter(|line| !line.is_empty());
    let secret = lines.next().filter(|line| !line.is_empty());
    match (id, secret) {
        (Some(id), Some(secret)) => Ok((id.to_owned(), secret.to_owned())),
        _ => Err(LINES_FORM),
    }
}

/// The text of the file that the flag `flag` names, if only its owner can read it.
///
/// # Errors
/// It cannot be read, or a group or other permission bit is set; led by the flag and the path.
pub(super) fn read_private_text(flag: &str, path: &Path) -> Result<String, String> {
    let shown = path.display();
    let fail = |why: &str| format!("{flag} {shown}: {why}");
    // The link itself, not what it points at: a link is not a key file.
    let meta = std::fs::symlink_metadata(path);
    let meta = meta.map_err(|why| fail(&why.to_string()))?;
    if meta.file_type().is_symlink() {
        return Err(fail("is a symbolic link; name the file itself"));
    }
    if meta.uid() != rustix::process::geteuid().as_raw() {
        return Err(fail("is not owned by the user the server runs as"));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(fail("others can read it; make it mode 0600"));
    }
    let text = std::fs::read_to_string(path);
    text.map_err(|why| fail(&why.to_string()))
}

/// The access key id and the secret access key in the file that the flag `flag` names, in
/// either form (see the module docs).
///
/// # Errors
/// What is wrong with the file, led by the flag and the path: it cannot be read, a group or
/// other permission bit is set, or its text is not a key in either form.
pub(super) fn read_credentials(flag: &str, path: &Path) -> Result<(String, String), String> {
    let fail = |why: &str| format!("{flag} {}: {why}", path.display());
    let text = read_private_text(flag, path)?;
    let json = text.trim_start().starts_with('{');
    let pair = if json {
        from_json(&text)
    } else {
        from_lines(&text)
    };
    pair.map_err(fail)
}

#[cfg(test)]
#[path = "keys_tests.rs"]
mod tests;
