//! The scoped key: a header-signed key that reads, creates and deletes under prefixes the key
//! file lists, and does nothing else.
//!
//! A server has any number of them (`--scoped-key-file`, one file per key, mode 0600, the file
//! itself and owned by the user the server runs as):
//!
//! ```json
//! {"access_key_id": "frontend-packs", "secret_access_key": "...",
//!  "read_prefixes": ["cas/"],
//!  "write_prefixes": ["cas/f/fe1/"],
//!  "delete_prefixes": []}
//! ```
//!
//! All five fields are required and no other is allowed. Each prefix ends in `/` and is not
//! `_journal/` or `_tiering/`, under them, a prefix of them, or a path with one of those names as
//! a segment. The three lists may overlap, a list may be empty, and a key whose three lists are
//! all empty is refused (it could do nothing). A request signed by the key is allowed for the
//! calls below, and only for a key (or a list prefix) that starts with a prefix of the list the
//! call needs and, for a key, is longer than it, after the path is percent-decoded and with no
//! `..`, no `.` segment and no control character in it:
//!
//! - **read**: GetObject and HeadObject of such a key; ListObjectsV2 and ListMultipartUploads
//!   (`GET ?uploads`) with a `prefix` parameter that starts with a read prefix (a list with no
//!   prefix, or one outside, is refused and not filtered);
//! - **write**: a create-only PutObject (no `If-Match`, and `If-None-Match` absent or `*`), and
//!   the calls of a multipart upload of such a key: CreateMultipartUpload, UploadPart,
//!   CompleteMultipartUpload and AbortMultipartUpload;
//! - **delete**: DeleteObject of such a key.
//!
//! Everything else is `403 AccessDenied`, answered before the body is read: a replacing put
//! (`If-Match`), a copy (`x-amz-copy-source`, on a put or on a part), a call on the bucket itself
//! other than a listing, any method but `GET`, `HEAD`, `PUT`, `POST` and `DELETE`, a key or a
//! listing under `_journal/` or `_tiering/` (no prefix of a key can cover them), and a call on a
//! key outside the list its kind needs: a read prefix does not let the key write, and a write
//! prefix does not let it read. A put over a key that exists is `412` as for every create-only
//! put. The key is refused on a presigned URL (the URL's key must be a presigning key).
//!
//! The refusals come in the order of the GC key's: the signature and the key id, then the method,
//! then the path and query are decoded, then the checksum headers of the request are read, and
//! last the key, the list prefix and the headers are held to the lists. A call that is both
//! refused and malformed may be answered with the malformed-request error rather than 403.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hyper::Method;
use hyper::http::request::Parts;
use serde::Deserialize;

use crate::journal::{JOURNAL_PREFIX, TIERING_PREFIX};

use super::ServerConfig;
use super::auth::Refusal;
use super::gc::{check_credentials, ensure_ids_distinct};
use super::keys::{AccessKey, read_private_text};
use super::ops::{MULTIPART, creates_only};
use super::presign::{PresignError, check_name, validate_prefix};
use super::request::{Query, Target, header};

/// The flag whose files hold the scoped keys.
const FLAG: &str = "--scoped-key-file";

pub(crate) const NOT_ALLOWED: Refusal = Refusal {
    code: "AccessDenied",
    message: "Access Denied: this access key may not make this call: it reads, creates and deletes under its prefixes only, and does not replace or copy.",
};

pub(crate) const OUT_OF_SCOPE: Refusal = Refusal {
    code: "AccessDenied",
    message: "Access Denied: the key, or the listing prefix, is outside the prefixes this access key has for this call.",
};

/// The prefixes a scoped key has, by what each lets it do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Scopes {
    read: Vec<String>,
    write: Vec<String>,
    delete: Vec<String>,
}

impl Scopes {
    /// Whether `listed` is a prefix a listing may name: it starts with a prefix of the read list.
    ///
    /// # Errors
    /// [`OUT_OF_SCOPE`].
    pub(crate) fn may_list(&self, listed: &str) -> Result<(), Refusal> {
        covers(&self.read, listed, false)
    }
}

/// Whether `name` starts with a prefix of `list` (and, for a key, is longer than it) and is a
/// plain name. A name under a prefix of a list is never one of the proxy's: `Scopes` are made
/// only of prefixes that [`valid_prefix`] accepted, none of which is, covers or is under
/// `_journal/` or `_tiering/`.
fn covers(list: &[String], name: &str, key: bool) -> Result<(), Refusal> {
    let under =
        |prefix: &String| name.starts_with(prefix.as_str()) && (!key || name.len() > prefix.len());
    if list.iter().any(under) && check_name(name).is_ok() {
        Ok(())
    } else {
        Err(OUT_OF_SCOPE)
    }
}

/// Whether `prefix` is one a scoped key may have: a valid prefix ending in `/`, with no segment
/// called like a prefix of the proxy.
fn valid_prefix(prefix: &str) -> Result<(), PresignError> {
    if validate_prefix(prefix).is_err() || !prefix.ends_with('/') {
        return Err(PresignError::Key(
            "a prefix is not a valid prefix ending in /",
        ));
    }
    let reserved = [JOURNAL_PREFIX, TIERING_PREFIX].map(|name| name.trim_end_matches('/'));
    if prefix.split('/').any(|segment| reserved.contains(&segment)) {
        return Err(PresignError::Key(
            "a prefix is, or contains, _journal/ or _tiering/",
        ));
    }
    Ok(())
}

/// A key that reads, creates and deletes under the prefixes of three lists.
#[derive(Clone, PartialEq, Eq)]
pub struct ScopedKey {
    key: AccessKey,
    scopes: Arc<Scopes>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyFile {
    access_key_id: String,
    secret_access_key: String,
    read_prefixes: Vec<String>,
    write_prefixes: Vec<String>,
    delete_prefixes: Vec<String>,
}

/// The prefixes of a [`ScopedKey`], by what the key may do under them.
#[derive(Clone, Copy, Debug, Default)]
pub struct Prefixes<'a> {
    /// Where the key may read and list.
    pub read: &'a [&'a str],
    /// Where the key may create objects and uploads.
    pub write: &'a [&'a str],
    /// Where the key may delete objects.
    pub delete: &'a [&'a str],
}

impl ScopedKey {
    /// The key with this id and secret, limited to the prefixes it may read, create under and
    /// delete under.
    ///
    /// # Errors
    /// [`PresignError::Key`] for an empty id or secret, an id with a `/`, `,`, `=` or a
    /// character that is not printable ASCII, a prefix that is not valid, does not end in `/`, or
    /// is or contains `_journal/` or `_tiering/`, and three lists that are all empty.
    pub fn new(id: &str, secret: &str, prefixes: &Prefixes<'_>) -> Result<Self, PresignError> {
        check_credentials(id, secret)?;
        let Prefixes {
            read,
            write,
            delete,
        } = *prefixes;
        let lists = [read, write, delete];
        if lists.iter().all(|list| list.is_empty()) {
            return Err(PresignError::Key("the three prefix lists are all empty"));
        }
        for prefix in lists.iter().flat_map(|list| list.iter()) {
            valid_prefix(prefix)?;
        }
        let own = |list: &[&str]| list.iter().map(|prefix| (*prefix).to_owned()).collect();
        let scopes = Scopes {
            read: own(read),
            write: own(write),
            delete: own(delete),
        };
        Ok(Self {
            key: AccessKey::new(id, secret),
            scopes: Arc::new(scopes),
        })
    }

    /// The key in the file `path`: JSON of exactly `access_key_id`, `secret_access_key`,
    /// `read_prefixes`, `write_prefixes` and `delete_prefixes`, mode 0600 or tighter, the file
    /// itself, owned by the user the server runs as.
    ///
    /// # Errors
    /// What is wrong, led by `flag` and the path. No message carries the secret.
    pub fn from_file(flag: &str, path: &Path) -> Result<Self, String> {
        let fail = |why: &str| format!("{flag} {}: {why}", path.display());
        let text = read_private_text(flag, path)?;
        // The error of the parser is dropped: for a value of the wrong type it quotes the value.
        let parsed = serde_json::from_str::<KeyFile>(&text);
        let file = parsed.map_err(|_| {
            fail(
                "not JSON of exactly access_key_id, secret_access_key, read_prefixes, \
                 write_prefixes and delete_prefixes (the last three lists of strings)",
            )
        })?;
        let lists = [
            &file.read_prefixes,
            &file.write_prefixes,
            &file.delete_prefixes,
        ]
        .map(|list| list.iter().map(String::as_str).collect::<Vec<_>>());
        let [read, write, delete] = &lists;
        let prefixes = Prefixes {
            read,
            write,
            delete,
        };
        Self::new(&file.access_key_id, &file.secret_access_key, &prefixes)
            .map_err(|why| fail(&why.to_string()))
    }

    /// The id of the key.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.key.id
    }

    /// The prefixes the key may read under.
    #[must_use]
    pub fn read_prefixes(&self) -> &[String] {
        &self.scopes.read
    }

    /// The prefixes the key may create objects and uploads under.
    #[must_use]
    pub fn write_prefixes(&self) -> &[String] {
        &self.scopes.write
    }

    /// The prefixes the key may delete under.
    #[must_use]
    pub fn delete_prefixes(&self) -> &[String] {
        &self.scopes.delete
    }

    /// The secret of the key.
    pub(super) fn secret(&self) -> &str {
        &self.key.secret
    }

    /// The prefixes, to hand to the role of a request.
    pub(super) fn scopes(&self) -> Arc<Scopes> {
        Arc::clone(&self.scopes)
    }
}

impl fmt::Debug for ScopedKey {
    /// Shows the id and the prefixes and never the secret.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScopedKey")
            .field("id", &self.key.id)
            .field("scopes", &self.scopes)
            .finish_non_exhaustive()
    }
}

impl ServerConfig {
    /// The scoped key with this id, if the server has one.
    pub(super) fn scoped_key(&self, id: &str) -> Option<&ScopedKey> {
        self.scoped_keys.iter().find(|key| key.id() == id)
    }
}

/// The scoped keys in the files of the flag `--scoped-key-file`, in the order of the flags.
///
/// # Errors
/// The first file that cannot be used, as [`ScopedKey::from_file`] says.
pub(super) fn scoped_keys(paths: &[PathBuf]) -> Result<Vec<ScopedKey>, String> {
    paths
        .iter()
        .map(|path| ScopedKey::from_file(FLAG, path))
        .collect()
}

/// The ids of the scoped keys, for the check that no two keys of a server share one.
pub(super) fn ids(keys: &[ScopedKey]) -> Vec<&str> {
    keys.iter().map(ScopedKey::id).collect()
}

/// Refuses a server in which two keys have one id: a request names its key by id alone.
///
/// # Errors
/// What is wrong, naming the id (never a secret).
pub(super) fn ensure_distinct(others: &[&str], scoped: &[ScopedKey]) -> Result<(), String> {
    ensure_ids_distinct(others, scoped.iter().map(ScopedKey::id))
}

/// Whether the call is one a key with these scopes may do.
///
/// # Errors
/// [`NOT_ALLOWED`] for a call of another kind, a copy, or a put that replaces; [`OUT_OF_SCOPE`]
/// for a key or a list prefix that is not inside the list the call needs.
pub(super) fn permit(
    scopes: &Scopes,
    parts: &Parts,
    target: &Target,
    query: &Query,
) -> Result<(), Refusal> {
    if header(&parts.headers, "x-amz-copy-source").is_some() {
        return Err(NOT_ALLOWED);
    }
    let multipart = query.has_any(&MULTIPART);
    match (&parts.method, target, multipart) {
        (&Method::GET, Target::Bucket, _) => {
            let listed = query.get("prefix").ok_or(OUT_OF_SCOPE)?;
            scopes.may_list(listed)
        }
        (&Method::GET | &Method::HEAD, Target::Object(key), false) => {
            covers(&scopes.read, key, true)
        }
        (&Method::PUT, Target::Object(key), false) => {
            covers(&scopes.write, key, true)?;
            if creates_only(&parts.headers) {
                Ok(())
            } else {
                Err(NOT_ALLOWED)
            }
        }
        (&Method::PUT | &Method::POST | &Method::DELETE, Target::Object(key), true) => {
            covers(&scopes.write, key, true)
        }
        (&Method::DELETE, Target::Object(key), false) => covers(&scopes.delete, key, true),
        _ => Err(NOT_ALLOWED),
    }
}
