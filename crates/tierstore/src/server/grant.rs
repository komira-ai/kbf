//! The read grant: a short signed token that lets a request `GET` or `HEAD` the objects it lists,
//! and nothing else.
//!
//! A grant is for a process that holds no S3 key and must read a known set of objects for a short
//! time (a build job reading the packs it needs). The process that mints it ([`mint_grant`]) holds
//! a grant key; the server holds the same key (`--grant-key-file`, [`ServerConfig::grant_keys`]),
//! checks the grant once per request, and allows a `GET` or `HEAD`, with any `Range`, of a listed
//! object. Every other request that carries a grant is `403 AccessDenied`, answered before its
//! body is read: another object, another method (a grant never writes, deletes or lists), a call
//! on the bucket, a multipart call, a query parameter but `x-id`, an expired grant, a grant for
//! another bucket, a grant whose signature does not match or whose key id the server does not
//! know. A request that carries both a grant and an `Authorization` header is refused too: a
//! request has one credential. The grant works wherever the S3 front does: on a storage node and
//! on a build node.
//!
//! # The token
//!
//! The request header is `x-tierstore-grant: <token>`, once. The token is
//!
//! ```text
//! token = base64url(body) "." base64url(tag)
//! tag   = HMAC-SHA256(key = the secret of the grant key, message = body)
//! body  = "tierstore-grant-v1" LF key-id LF bucket LF expires LF name { LF name }
//! ```
//!
//! `base64url` is the URL-safe alphabet of RFC 4648 section 5, without padding and without
//! trailing bits that are not zero. `LF` is the byte 0x0A; the body has none at its end. `key-id`
//! is the access key id of the grant key (printable ASCII, no `/`, `,` or `=`). `bucket` is the
//! bucket the server serves. `expires` is the unix time in seconds at which the grant ends, in
//! decimal digits without a sign or a leading zero: the grant is good while the server's clock
//! reads less than that. Each `name` is the exact key of an object (at most 1024 bytes, UTF-8, no
//! control character, no `..`, no `.` segment, not under `_journal/` or `_tiering/`, not empty);
//! there is at least one and at most [`MAX_GRANT_KEYS`], in strictly ascending order of their
//! bytes (so none twice). The header value is at most [`MAX_TOKEN_BYTES`] bytes.
//!
//! The server finds the key by `key-id`, checks the tag in constant time over the body exactly as
//! it was sent, and only then reads the rest. A byte changed anywhere in the body, or a tag made
//! with another secret, is refused.

use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::Mac;
use hyper::http::request::Parts;

use crate::journal::is_reserved;

use super::ServerConfig;
use super::auth::{Refusal, Signed, keyed};
use super::gc::{check_credentials, ensure_ids_distinct};
use super::keys::{AccessKey, Role, read_credentials};
use super::presign::check_name;
use super::request::{Query, Target};

/// The header that carries the token.
pub const GRANT_HEADER: &str = "x-tierstore-grant";

/// The longest value of the header, in bytes.
pub const MAX_TOKEN_BYTES: usize = 64 * 1024;

/// The most objects a grant lists.
pub const MAX_GRANT_KEYS: usize = 4096;

/// The first line of a body: it names the format, so that a tag made for another purpose with the
/// same secret is not one of a grant.
const MAGIC: &str = "tierstore-grant-v1";

/// The flag whose files hold the grant keys.
const FLAG: &str = "--grant-key-file";

/// What a request that carries a grant has as the hash of its body: it signs none.
const UNSIGNED: &str = "UNSIGNED-PAYLOAD";

pub(crate) const NOT_ALLOWED: Refusal = Refusal {
    code: "AccessDenied",
    message: "Access Denied: a read grant lets a request GET or HEAD an object it lists, and nothing else.",
};

pub(crate) const NOT_LISTED: Refusal = Refusal {
    code: "AccessDenied",
    message: "Access Denied: the object is not one the read grant lists.",
};

const MALFORMED: Refusal = Refusal {
    code: "AccessDenied",
    message: "Access Denied: the read grant is malformed.",
};

const TOO_LARGE: Refusal = Refusal {
    code: "AccessDenied",
    message: "Access Denied: the read grant is larger than the server takes.",
};

const UNTRUSTED: Refusal = Refusal {
    code: "AccessDenied",
    message: "Access Denied: the key id of the read grant is not known, or its signature does not match.",
};

const EXPIRED: Refusal = Refusal {
    code: "AccessDenied",
    message: "Access Denied: the read grant has expired.",
};

const OTHER_BUCKET: Refusal = Refusal {
    code: "AccessDenied",
    message: "Access Denied: the read grant is for another bucket.",
};

const TWO_CREDENTIALS: Refusal = Refusal {
    code: "AccessDenied",
    message: "Access Denied: a request carries a read grant or an Authorization header, not both.",
};

/// Why a grant cannot be made.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GrantError {
    /// The key id or the secret is empty, or the id has a `/`, `,`, `=` or a character that is
    /// not printable ASCII.
    #[error("the grant key has an empty or odd id or secret")]
    Key,
    /// The bucket name is empty or has a control character.
    #[error("the bucket name is empty or has a control character")]
    Bucket,
    /// The grant lists no object.
    #[error("a grant lists at least one object")]
    NoKeys,
    /// The grant lists more objects than [`MAX_GRANT_KEYS`].
    #[error("a grant lists too many objects: {count}")]
    TooManyKeys {
        /// How many objects were given, counting each name once.
        count: usize,
    },
    /// A name is not an object key a grant may list.
    #[error("{name:?} cannot be granted: {why}")]
    Name {
        /// The name.
        name: String,
        /// What is wrong with it.
        why: String,
    },
    /// The token would be longer than [`MAX_TOKEN_BYTES`].
    #[error("the token would be {len} bytes long")]
    TooLarge {
        /// The length of the token.
        len: usize,
    },
}

/// Makes the token that lets a request to the bucket `bucket` read the objects `names` until the
/// unix time `expires_at` (seconds), signed with the grant key `key`. The names are sorted, and
/// one given twice is listed once.
///
/// # Errors
/// [`GrantError`]: a key, a bucket or a name the server would refuse, no name, too many names, or
/// a token over [`MAX_TOKEN_BYTES`].
pub fn mint_grant<S: AsRef<str>>(
    key: &AccessKey,
    bucket: &str,
    expires_at: u64,
    names: impl IntoIterator<Item = S>,
) -> Result<String, GrantError> {
    let given: Vec<S> = names.into_iter().collect();
    mint_sorted(
        key,
        bucket,
        expires_at,
        given.iter().map(AsRef::as_ref).collect(),
    )
}

/// The body of [`mint_grant`], not generic over the type of the names, so that one instance of it
/// holds every refusal.
fn mint_sorted(
    key: &AccessKey,
    bucket: &str,
    expires_at: u64,
    mut names: Vec<&str>,
) -> Result<String, GrantError> {
    check_credentials(&key.id, &key.secret).map_err(|_| GrantError::Key)?;
    check_bucket(bucket)?;
    names.sort_unstable();
    names.dedup();
    check_names(&names)?;
    let id = &key.id;
    let body = format!(
        "{MAGIC}\n{id}\n{bucket}\n{expires_at}\n{}",
        names.join("\n")
    );
    let token = seal(&key.secret, body.as_bytes());
    if token.len() > MAX_TOKEN_BYTES {
        return Err(GrantError::TooLarge { len: token.len() });
    }
    Ok(token)
}

/// The token for `body`: the body and its tag under `secret`.
pub(super) fn seal(secret: &str, body: &[u8]) -> String {
    let mut mac = keyed(secret.as_bytes());
    mac.update(body);
    let tag = mac.finalize().into_bytes();
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(body),
        URL_SAFE_NO_PAD.encode(tag)
    )
}

fn check_bucket(bucket: &str) -> Result<(), GrantError> {
    if bucket.is_empty() || bucket.chars().any(char::is_control) {
        return Err(GrantError::Bucket);
    }
    Ok(())
}

/// Why `name` is not an object key a grant may list, if it is not.
fn refusal_of(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("empty".to_owned());
    }
    if is_reserved(name) {
        return Some("a key of the proxy".to_owned());
    }
    check_name(name).err().map(|why| why.to_string())
}

/// Holds the names to what a body has: some, not too many, each a valid key, in strictly
/// ascending order.
fn check_names(names: &[&str]) -> Result<(), GrantError> {
    if names.is_empty() {
        return Err(GrantError::NoKeys);
    }
    if names.len() > MAX_GRANT_KEYS {
        return Err(GrantError::TooManyKeys { count: names.len() });
    }
    for name in names {
        if let Some(why) = refusal_of(name) {
            return Err(GrantError::Name {
                name: (*name).to_owned(),
                why,
            });
        }
    }
    if let Some(pair) = names.windows(2).find(|pair| pair[0] >= pair[1]) {
        return Err(GrantError::Name {
            name: pair[1].to_owned(),
            why: "not in strictly ascending order".to_owned(),
        });
    }
    Ok(())
}

/// The fields of a body, before its tag is checked.
struct Fields<'a> {
    key_id: &'a str,
    bucket: &'a str,
    expires: u64,
    names: Vec<&'a str>,
}

impl<'a> Fields<'a> {
    /// The fields of `body`, if it has the shape of one.
    fn parse(body: &'a [u8]) -> Option<Self> {
        let text = std::str::from_utf8(body).ok()?;
        let lines: Vec<&str> = text.splitn(5, '\n').collect();
        let [magic, key_id, bucket, expires, rest] = lines[..] else {
            return None;
        };
        if magic != MAGIC {
            return None;
        }
        // The number as it is written back, so that a sign or a leading zero is refused.
        let number = expires.parse::<u64>().ok();
        let number = number.filter(|number| number.to_string() == expires)?;
        Some(Self {
            key_id,
            bucket,
            expires: number,
            names: rest.split('\n').collect(),
        })
    }
}

/// The grant a request carries, when it carries one: checks it and gives the role it has, which
/// holds the names the request may read.
///
/// # Errors
/// The [`Refusal`] for a grant that is malformed, too large, not signed by a key of the server,
/// expired, or for another bucket, and for a request that carries an `Authorization` header too.
pub(super) fn verify(config: &ServerConfig, parts: &Parts) -> Result<Signed, Refusal> {
    if parts.headers.contains_key("authorization") {
        return Err(TWO_CREDENTIALS);
    }
    let mut values = parts.headers.get_all(GRANT_HEADER).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return Err(MALFORMED);
    };
    if value.len() > MAX_TOKEN_BYTES {
        return Err(TOO_LARGE);
    }
    let token = value.to_str().map_err(|_| MALFORMED)?;
    let (body, tag) = token.split_once('.').ok_or(MALFORMED)?;
    let body = URL_SAFE_NO_PAD.decode(body).map_err(|_| MALFORMED)?;
    let tag = URL_SAFE_NO_PAD.decode(tag).map_err(|_| MALFORMED)?;
    let fields = Fields::parse(&body).ok_or(MALFORMED)?;
    let key = config.grant_keys.iter().find(|key| key.id == fields.key_id);
    let key = key.ok_or(UNTRUSTED)?;
    let mut mac = keyed(key.secret.as_bytes());
    mac.update(&body);
    mac.verify_slice(&tag).map_err(|_| UNTRUSTED)?;
    // The body is the key's from here on.
    check_names(&fields.names).map_err(|_| MALFORMED)?;
    if fields.bucket != config.bucket {
        return Err(OTHER_BUCKET);
    }
    if fields.expires.saturating_mul(1000) <= config.clock.now() {
        return Err(EXPIRED);
    }
    let names = fields.names.iter().map(|name| (*name).to_owned()).collect();
    Ok(Signed {
        payload: UNSIGNED.to_owned(),
        role: Role::Grant(Arc::new(names)),
        presign: None,
    })
}

/// Whether the call is one a grant that lists `names` (in ascending order) allows. The method
/// has been checked by the role: it is a `GET` or a `HEAD`.
///
/// # Errors
/// [`NOT_ALLOWED`] for a call on the bucket or the service, one with a query parameter but
/// `x-id` (a multipart call, a listing), [`NOT_LISTED`] for an object the grant does not list.
pub(super) fn permit(names: &[String], target: &Target, query: &Query) -> Result<(), Refusal> {
    let Target::Object(key) = target else {
        return Err(NOT_ALLOWED);
    };
    if !query.only(&["x-id"]) {
        return Err(NOT_ALLOWED);
    }
    let found = names.binary_search_by(|listed| listed.as_str().cmp(key.as_str()));
    found.map(drop).map_err(|_| NOT_LISTED)
}

/// The grant keys in the files of the flag `--grant-key-file`, in the order of the flags. A file
/// holds one key as the other credentials files do: JSON of exactly `access_key_id` and
/// `secret_access_key`, or the id and the secret on two lines; mode 0600, the file itself, owned
/// by the user the server runs as.
///
/// # Errors
/// The first file that cannot be used, as the credentials files say.
pub(super) fn grant_keys(paths: &[PathBuf]) -> Result<Vec<AccessKey>, String> {
    let pairs = paths.iter().map(|path| read_credentials(FLAG, path));
    pairs
        .map(|pair| pair.map(|(id, secret)| AccessKey { id, secret }))
        .collect()
}

/// The ids of the grant keys, for the check that no two keys of a server share one.
pub(super) fn ids(keys: &[AccessKey]) -> Vec<&str> {
    keys.iter().map(|key| key.id.as_str()).collect()
}

/// Refuses a server in which two keys have one id.
///
/// # Errors
/// What is wrong, naming the id (never a secret).
pub(super) fn ensure_distinct(others: &[&str], keys: &[AccessKey]) -> Result<(), String> {
    ensure_ids_distinct(others, keys.iter().map(|key| key.id.as_str()))
}

#[cfg(test)]
#[path = "grant_tests.rs"]
mod tests;
