//! The garbage-collection key: a header-signed key that lists, heads and deletes under one
//! prefix and does nothing else.
//!
//! A server has any number of them (`--gc-key-file`, one file per key, mode 0600, the file
//! itself and owned by the user the server runs as):
//!
//! ```json
//! {"access_key_id": "pack-gc", "secret_access_key": "...", "prefix": "cas/b/"}
//! ```
//!
//! All three fields are required and no other is allowed. The prefix ends in `/` and is not
//! `_journal/` or `_tiering/` or under them or a prefix of them. A request signed by the key is
//! allowed, and only for a key (or a list prefix) that starts with the prefix of the key and is
//! longer than it, after the path is percent-decoded and with no `..`, no `.` segment and no
//! control character in it:
//!
//! - ListObjectsV2 with a `prefix` parameter inside the prefix of the key (a list with no prefix,
//!   or one outside, is refused and not filtered), and ListMultipartUploads (`GET ?uploads`) with
//!   such a prefix;
//! - HeadObject;
//! - DeleteObject, and AbortMultipartUpload of an upload of such a key.
//!
//! Everything else is `403 AccessDenied` before the body is read: GetObject (a GC key never reads
//! a body), every `PUT` and `POST` (put, part, start and completion of an upload, copy), the calls
//! on the bucket itself, and every key under `_journal/` or `_tiering/`. The key is refused
//! on a presigned URL (the URL's key must be a presigning key), as a presigning key is refused in
//! the header.
//!
//! A delete through the proxy removes the object from both tiers and journals the delete, as for
//! any key. **A delete by a GC key is not create-only: it is the only way an object leaves
//! `cas/b/`**, since the presigning key can only create, and the full key is not meant for the
//! frontend.

use std::fmt;
use std::path::{Path, PathBuf};

use hyper::Method;
use hyper::http::request::Parts;
use serde::Deserialize;

use crate::journal::is_reserved;

use super::ServerConfig;
use super::auth::Refusal;
use super::keys::{AccessKey, read_private_text};
use super::presign::{PresignError, check_name, validate_prefix};
use super::request::{Query, Target};

/// The flag whose files hold the GC keys.
const FLAG: &str = "--gc-key-file";

/// The query parameters that make a request part of a multipart upload.
const MULTIPART: [&str; 3] = ["partNumber", "uploadId", "uploads"];

pub(crate) const NOT_ALLOWED: Refusal = Refusal {
    code: "AccessDenied",
    message: "Access Denied: this access key lists, heads and deletes under its prefix only.",
};

pub(crate) const OUT_OF_SCOPE: Refusal = Refusal {
    code: "AccessDenied",
    message: "Access Denied: the key, or the listing prefix, is outside the prefix of this access key.",
};

/// A key that lists, heads and deletes under one prefix.
#[derive(Clone, PartialEq, Eq)]
pub struct GcKey {
    key: AccessKey,
    prefix: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyFile {
    access_key_id: String,
    secret_access_key: String,
    prefix: String,
}

impl GcKey {
    /// The key with this id and secret, limited to `prefix`.
    ///
    /// # Errors
    /// [`PresignError::Key`] for an empty id or secret, an id with a `/`, `,`, `=` or a character
    /// that is not printable ASCII, and a prefix that does not end in `/` or is not a valid
    /// prefix (see [`validate_prefix`]).
    pub fn new(id: &str, secret: &str, prefix: &str) -> Result<Self, PresignError> {
        let plain = |c: char| c.is_ascii_graphic() && !"/,=".contains(c);
        if id.is_empty() || secret.is_empty() || !id.chars().all(plain) {
            return Err(PresignError::Key(
                "the id or the secret is empty, or the id is odd",
            ));
        }
        if validate_prefix(prefix).is_err() || !prefix.ends_with('/') {
            return Err(PresignError::Key(
                "the prefix is not a valid prefix ending in /",
            ));
        }
        Ok(Self {
            key: AccessKey::new(id, secret),
            prefix: prefix.to_owned(),
        })
    }

    /// The key in the file `path`: JSON of exactly `access_key_id`, `secret_access_key` and
    /// `prefix`, mode 0600 or tighter, the file itself, owned by the user the server runs as.
    ///
    /// # Errors
    /// What is wrong, led by `flag` and the path. No message carries the secret.
    pub fn from_file(flag: &str, path: &Path) -> Result<Self, String> {
        let fail = |why: &str| format!("{flag} {}: {why}", path.display());
        let text = read_private_text(flag, path)?;
        // The error of the parser is dropped: for a value of the wrong type it quotes the value.
        let parsed = serde_json::from_str::<KeyFile>(&text);
        let file = parsed
            .map_err(|_| fail("not JSON of exactly access_key_id, secret_access_key and prefix"))?;
        Self::new(&file.access_key_id, &file.secret_access_key, &file.prefix)
            .map_err(|why| fail(&why.to_string()))
    }

    /// The id of the key.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.key.id
    }

    /// The prefix the key is limited to.
    #[must_use]
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// The secret of the key.
    pub(super) fn secret(&self) -> &str {
        &self.key.secret
    }
}

impl fmt::Debug for GcKey {
    /// Shows the id and the prefix and never the secret.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GcKey")
            .field("id", &self.key.id)
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl ServerConfig {
    /// The GC key with this id, if the server has one.
    pub(super) fn gc_key(&self, id: &str) -> Option<&GcKey> {
        self.gc_keys.iter().find(|key| key.id() == id)
    }
}

/// The GC keys in the files of the flag `--gc-key-file`, in the order of the flags.
///
/// # Errors
/// The first file that cannot be used, as [`GcKey::from_file`] says.
pub(super) fn gc_keys(paths: &[PathBuf]) -> Result<Vec<GcKey>, String> {
    paths
        .iter()
        .map(|path| GcKey::from_file(FLAG, path))
        .collect()
}

/// The ids of the GC keys, for the check that no two keys of a server share one.
pub(super) fn ids(keys: &[GcKey]) -> Vec<&str> {
    keys.iter().map(GcKey::id).collect()
}

/// Whether the call is one a GC key of `prefix` may do.
///
/// # Errors
/// [`NOT_ALLOWED`] for a call of another kind, [`OUT_OF_SCOPE`] for a key or a list prefix that is
/// not inside the prefix.
pub(super) fn permit(
    prefix: &str,
    parts: &Parts,
    target: &Target,
    query: &Query,
) -> Result<(), Refusal> {
    // `GcKey::new` also refuses a prefix that is, or covers, `_journal/` or `_tiering/`, so a name
    // under the prefix of a key is never one of the proxy's; the check of reserved names here is the
    // second guard, for a caller that builds its prefix another way.
    let inside = |name: &str| {
        let longer = name.len() > prefix.len() && name.starts_with(prefix);
        if longer && !is_reserved(name) && check_name(name).is_ok() {
            Ok(())
        } else {
            Err(OUT_OF_SCOPE)
        }
    };
    let within = |listed: &str| {
        let reserved = is_reserved(listed);
        if listed.starts_with(prefix) && !reserved && check_name(listed).is_ok() {
            Ok(())
        } else {
            Err(OUT_OF_SCOPE)
        }
    };
    let multipart = query.has_any(&MULTIPART);
    match (&parts.method, target, multipart) {
        (&Method::GET, Target::Bucket, _) => {
            let listed = query.get("prefix").ok_or(OUT_OF_SCOPE)?;
            within(listed)
        }
        (&Method::HEAD, Target::Object(key), false) | (&Method::DELETE, Target::Object(key), _) => {
            inside(key)
        }
        _ => Err(NOT_ALLOWED),
    }
}

/// Refuses a server in which two keys have one id: a request names its key by id alone.
///
/// # Errors
/// What is wrong, naming the id (never a secret).
pub(super) fn ensure_distinct(others: &[&str], gc: &[GcKey]) -> Result<(), String> {
    let mut seen: std::collections::BTreeSet<&str> = others.iter().copied().collect();
    for key in gc {
        if !seen.insert(key.id()) {
            let id = key.id();
            return Err(format!(
                "the access key id {id:?} is used twice among the key flags"
            ));
        }
    }
    Ok(())
}
