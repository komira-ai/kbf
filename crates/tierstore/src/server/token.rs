//! Signed, short-lived, prefix-scoped tokens, which the front checks without keeping any state.
//!
//! A token is a SigV4 credential, so that any S3 client works with it unchanged given the pair:
//! its access key id carries the claims, and its secret access key is worked out from the claims
//! with a minting key that the minter and the server share. The server re-derives the secret from
//! the id, then checks the signature of the request as it does for any key.
//!
//! # The access key id
//!
//! `tt1` followed by the base64url of these bytes, without padding (the digit is the version of
//! the format, and is covered by the MAC):
//!
//! ```text
//! role        1 byte    0 client, 1 gc
//! ops         1 byte    bit 0 create, bit 1 read, bit 2 delete; no other bit
//! key id len  1 byte    1 to 32
//! key id      n bytes   the id of the minting key: ASCII letters, digits, `.`, `_` and `-`
//! not after   8 bytes   big-endian, in seconds since the Unix epoch
//! prefix len  2 bytes   big-endian, 1 to 256
//! prefix      m bytes   UTF-8: the bytes that an object key must start with
//! mac         16 bytes  the first 16 bytes of HMAC-SHA256(minting key, "tierstore/token/mac\0"
//!                       || "tt1" || every byte above)
//! ```
//!
//! The secret access key is the lower-case hex of HMAC-SHA256(minting key,
//! "tierstore/token/secret\0" || "tt1" || every byte above but the MAC), 64 characters. It is
//! never in the id, and the MAC does not give it away: the two are separate derivations.
//!
//! # What a token allows
//!
//! - `create`: PutObject (create-only, as everything the proxy takes is) and the calls of a
//!   multipart upload (start, part, completion, abort) on a key that starts with the prefix;
//! - `read`: GetObject and HeadObject on a key that starts with the prefix, and ListObjectsV2
//!   with a `prefix` parameter that starts with the prefix of the token (a listing with another
//!   `prefix`, or with none, is refused and not filtered);
//! - `delete`: DeleteObject on a key that starts with the prefix, in a token of the role `gc`
//!   only;
//! - HeadBucket, with any token.
//!
//! A key under `_journal/` or `_tiering/` is never reachable, a key that is not a valid name (see
//! [`check_name`]) is refused, and a token is refused from its `not after` second on, with no
//! grace. Any other call is refused.

use std::fmt;
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use hyper::Method;
use hyper::http::request::Parts;
use serde::Deserialize;
use sha2::Sha256;

use crate::journal::{JOURNAL_PREFIX, TIERING_PREFIX, is_reserved};

use super::auth::Refusal;
use super::keys::read_private_text;
use super::request::{Query, Target, header};

/// What an access key id of a token starts with: `tt` and the version of the format.
pub const MARKER: &str = "tt1";

/// The most bytes in the prefix of a token.
pub const MAX_PREFIX: usize = 256;

/// The most bytes in the id of a minting key.
pub const MAX_KEY_ID: usize = 32;

/// The fewest bytes in the secret of a minting key.
pub const MIN_KEY_SECRET: usize = 32;

/// The most bytes in an object key, as S3 has it.
const MAX_NAME: usize = 1024;

/// The bytes of the MAC in an id.
const MAC_LEN: usize = 16;

const MAC_LABEL: &[u8] = b"tierstore/token/mac\0";
const SECRET_LABEL: &[u8] = b"tierstore/token/secret\0";

const CREATE_BIT: u8 = 1;
const READ_BIT: u8 = 2;
const DELETE_BIT: u8 = 4;

/// The query parameters that make a request part of a multipart upload.
const MULTIPART: [&str; 3] = ["partNumber", "uploadId", "uploads"];

/// The role of a token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenRole {
    /// A writer or reader of its prefix: it cannot delete.
    Client,
    /// The garbage collector: the one role that may hold the delete operation.
    Gc,
}

/// The operations a token allows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ops {
    /// Create-only put and multipart upload.
    pub create: bool,
    /// Get, head and list.
    pub read: bool,
    /// Delete (only with the role [`TokenRole::Gc`]).
    pub delete: bool,
}

impl Ops {
    fn bits(self) -> u8 {
        let when = |on: bool, mask: u8| if on { mask } else { 0 };
        when(self.create, CREATE_BIT) | when(self.read, READ_BIT) | when(self.delete, DELETE_BIT)
    }

    fn from_bits(bits: u8) -> Option<Self> {
        (bits & !(CREATE_BIT | READ_BIT | DELETE_BIT) == 0).then_some(Self {
            create: bits & CREATE_BIT != 0,
            read: bits & READ_BIT != 0,
            delete: bits & DELETE_BIT != 0,
        })
    }
}

/// What a token says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claims {
    /// The role.
    pub role: TokenRole,
    /// The operations.
    pub ops: Ops,
    /// The bytes that a key must start with, for a call to be on it.
    pub prefix: String,
    /// The second, since the Unix epoch, from which the token is refused.
    pub not_after: u64,
}

/// Why a token is not valid, or cannot be made.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    /// The prefix or a key is not a name a token may cover; the text says why.
    #[error("not a valid name: {0}")]
    Name(&'static str),
    /// The claims are not ones a token may carry; the text says why.
    #[error("invalid claims: {0}")]
    Claims(&'static str),
    /// The minting key is not a usable key; the text says why.
    #[error("invalid minting key: {0}")]
    Key(&'static str),
    /// The access key id is not the id of a token.
    #[error("not a token")]
    Malformed,
    /// The id names a minting key that the server does not hold.
    #[error("unknown minting key id")]
    UnknownKeyId,
    /// The MAC of the id is not the one the minting key gives.
    #[error("the MAC does not match")]
    BadMac,
    /// The claims are more than the ceiling of the minting key allows; the text says which.
    #[error("beyond the ceiling of the minting key: {0}")]
    Exceeds(&'static str),
}

/// A key that tokens are signed with: an id, which a token names, and secret bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct MintingKey {
    id: String,
    secret: Vec<u8>,
    ceiling: Ceiling,
}

/// The most that tokens of one minting key may carry. A MAC proves that a holder of the key made
/// the claims, not that the key's holder may make them; the server holds every token to the
/// ceiling of the key it names, so that the key of one minter cannot mint another's rights. A key
/// has no default ceiling: one without is not accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ceiling {
    /// The prefix the prefix of a token must start with.
    pub prefix: String,
    /// The operations a token may have.
    pub ops: Ops,
    /// The most seconds, from the time of the request, that a token may have left to live.
    pub max_ttl_secs: u64,
    /// The most bytes of a request body (a put, a part) under a token of this key, if limited. A
    /// multipart upload as a whole is not bounded by it: it bounds each part.
    pub max_object_bytes: Option<u64>,
}

impl Ceiling {
    /// Whether the ceiling is one a key may have.
    ///
    /// # Errors
    /// [`TokenError::Key`], saying which part.
    pub fn validate(&self) -> Result<(), TokenError> {
        if validate_prefix(&self.prefix).is_err() {
            return Err(TokenError::Key("the ceiling prefix is not a valid prefix"));
        }
        if self.ops.bits() == 0 {
            return Err(TokenError::Key("the ceiling grants no operation"));
        }
        if self.max_ttl_secs == 0 {
            return Err(TokenError::Key("the ceiling ttl is zero"));
        }
        if self.max_object_bytes == Some(0) {
            return Err(TokenError::Key("the ceiling object size is zero"));
        }
        Ok(())
    }

    /// Whether the claims, at `now` (milliseconds), are within the ceiling.
    fn check(&self, claims: &Claims, now: u64) -> Result<(), TokenError> {
        if !claims.prefix.starts_with(&self.prefix) {
            return Err(TokenError::Exceeds("prefix"));
        }
        let (has, may) = (claims.ops, self.ops);
        if (has.create && !may.create) || (has.read && !may.read) || (has.delete && !may.delete) {
            return Err(TokenError::Exceeds("operations"));
        }
        let left = claims.not_after.saturating_mul(1000).saturating_sub(now);
        if left > self.max_ttl_secs.saturating_mul(1000) {
            return Err(TokenError::Exceeds("lifetime"));
        }
        Ok(())
    }
}

/// What a minting key file holds besides the id and the secret.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CeilingFile {
    prefix: String,
    ops: Vec<String>,
    max_ttl_secs: u64,
    max_object_bytes: Option<u64>,
}

/// What a minting key file holds.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyFile {
    access_key_id: String,
    secret_access_key: String,
    ceiling: CeilingFile,
}

impl CeilingFile {
    fn into_ceiling(self) -> Result<Ceiling, TokenError> {
        let mut ops = Ops::default();
        for name in &self.ops {
            match name.as_str() {
                "create" => ops.create = true,
                "read" => ops.read = true,
                "delete" => ops.delete = true,
                _ => {
                    return Err(TokenError::Key(
                        "an operation is not create, read or delete",
                    ));
                }
            }
        }
        Ok(Ceiling {
            prefix: self.prefix,
            ops,
            max_ttl_secs: self.max_ttl_secs,
            max_object_bytes: self.max_object_bytes,
        })
    }
}

impl MintingKey {
    /// The key with this id, these secret bytes and this ceiling.
    ///
    /// # Errors
    /// [`TokenError::Key`] for an id that is empty, over [`MAX_KEY_ID`] bytes, or has a byte
    /// that is not an ASCII letter, a digit, `.`, `_` or `-`, for a secret of fewer than
    /// [`MIN_KEY_SECRET`] bytes, and for a ceiling that [`Ceiling::validate`] refuses.
    pub fn new(id: &str, secret: &[u8], ceiling: Ceiling) -> Result<Self, TokenError> {
        let plain = |byte: u8| byte.is_ascii_alphanumeric() || b"._-".contains(&byte);
        if id.is_empty() || id.len() > MAX_KEY_ID || !id.bytes().all(plain) {
            return Err(TokenError::Key(
                "the id is 1 to 32 of letters, digits . _ -",
            ));
        }
        if secret.len() < MIN_KEY_SECRET {
            return Err(TokenError::Key("the secret is shorter than 32 bytes"));
        }
        ceiling.validate()?;
        Ok(Self {
            id: id.to_owned(),
            secret: secret.to_vec(),
            ceiling,
        })
    }

    /// The key in the file `path`: JSON of exactly `access_key_id`, `secret_access_key` and
    /// `ceiling`, which is `{"prefix": "...", "ops": ["create", "read", "delete"],
    /// "max_ttl_secs": 3600, "max_object_bytes": 1048576}` (`max_object_bytes` may be null; no
    /// other field, and none may be left out of the others). The file must be mode 0600 or
    /// tighter. `flag` is the name of the flag that named it, for the message. A file with no
    /// ceiling is refused.
    ///
    /// # Errors
    /// What is wrong with the file or the key, led by `flag` and the path. No message carries
    /// the secret.
    pub fn from_file(flag: &str, path: &Path) -> Result<Self, String> {
        let fail = |why: &str| format!("{flag} {}: {why}", path.display());
        let text = read_private_text(flag, path)?;
        // The error of the parser is dropped: for a value of the wrong type it quotes the value.
        let parsed = serde_json::from_str::<KeyFile>(&text);
        let file = parsed.map_err(|_| {
            fail("not JSON of exactly access_key_id, secret_access_key and a ceiling")
        })?;
        let ceiling = file
            .ceiling
            .into_ceiling()
            .map_err(|why| fail(&why.to_string()))?;
        Self::new(
            &file.access_key_id,
            file.secret_access_key.as_bytes(),
            ceiling,
        )
        .map_err(|why| fail(&why.to_string()))
    }

    /// The id of the key.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The ceiling of the key.
    #[must_use]
    pub fn ceiling(&self) -> &Ceiling {
        &self.ceiling
    }
}

impl fmt::Debug for MintingKey {
    /// Shows the id and never the secret.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MintingKey")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// The pair that a client signs with.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    /// The access key id: the claims and their MAC.
    pub access_key_id: String,
    /// The secret access key.
    pub secret_access_key: String,
}

impl fmt::Debug for Credential {
    /// Shows the id and never the secret.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("access_key_id", &self.access_key_id)
            .finish_non_exhaustive()
    }
}

/// Whether `text` is a name that a key or a prefix may be: at most 1024 bytes, no control
/// character (NUL included), no `..` anywhere and no `.` as a whole path segment. (A `..`
/// anywhere is refused and not only as a segment, so that no encoding of one gets by.)
///
/// # Errors
/// [`TokenError::Name`] with the reason.
pub fn check_name(text: &str) -> Result<(), TokenError> {
    if text.len() > MAX_NAME {
        return Err(TokenError::Name("over 1024 bytes"));
    }
    if text.chars().any(char::is_control) {
        return Err(TokenError::Name("a control character"));
    }
    if text.contains("..") {
        return Err(TokenError::Name("contains .."));
    }
    if text.split('/').any(|segment| segment == ".") {
        return Err(TokenError::Name("a . segment"));
    }
    Ok(())
}

/// Whether `prefix` is one a token may carry: a name, not empty, at most [`MAX_PREFIX`] bytes,
/// and neither under `_journal/` or `_tiering/` nor a prefix of either (so no key it matches is
/// one of the proxy's).
///
/// # Errors
/// [`TokenError::Name`] with the reason.
pub fn validate_prefix(prefix: &str) -> Result<(), TokenError> {
    if prefix.is_empty() {
        return Err(TokenError::Name("empty"));
    }
    if prefix.len() > MAX_PREFIX {
        return Err(TokenError::Name("over 256 bytes"));
    }
    check_name(prefix)?;
    let reserved = [JOURNAL_PREFIX, TIERING_PREFIX];
    if is_reserved(prefix) || reserved.iter().any(|name| name.starts_with(prefix)) {
        return Err(TokenError::Name("reserved"));
    }
    Ok(())
}

impl Claims {
    /// Whether the claims are ones a token may carry: a valid prefix, at least one operation, and
    /// the delete operation only with the role gc.
    ///
    /// # Errors
    /// [`TokenError`], saying which.
    pub fn validate(&self) -> Result<(), TokenError> {
        validate_prefix(&self.prefix)?;
        if self.ops.bits() == 0 {
            return Err(TokenError::Claims("no operation"));
        }
        if self.ops.delete && self.role != TokenRole::Gc {
            return Err(TokenError::Claims("delete needs the role gc"));
        }
        Ok(())
    }

    /// Whether the token has expired at `now` (milliseconds since the epoch): from its
    /// `not_after` second on, with no grace.
    #[must_use]
    pub fn expired(&self, now: u64) -> bool {
        now >= self.not_after.saturating_mul(1000)
    }

    /// The claims as the bytes of an id, MAC excluded.
    fn body(&self, key_id: &str) -> Vec<u8> {
        let role = match self.role {
            TokenRole::Client => 0,
            TokenRole::Gc => 1,
        };
        let mut bytes = vec![role, self.ops.bits()];
        bytes.push(u8::try_from(key_id.len()).expect("a minting key id is at most 32 bytes"));
        bytes.extend_from_slice(key_id.as_bytes());
        bytes.extend_from_slice(&self.not_after.to_be_bytes());
        let len = u16::try_from(self.prefix.len()).expect("a valid prefix is at most 256 bytes");
        bytes.extend_from_slice(&len.to_be_bytes());
        bytes.extend_from_slice(self.prefix.as_bytes());
        bytes
    }
}

/// HMAC-SHA256 of `label`, the marker and `body` under `key`.
fn derive(key: &MintingKey, label: &[u8], body: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&key.secret)
        .expect("HMAC accepts a key of any length");
    mac.update(label);
    mac.update(MARKER.as_bytes());
    mac.update(body);
    mac.finalize().into_bytes().to_vec()
}

fn secret_of(key: &MintingKey, body: &[u8]) -> String {
    hex::encode(derive(key, SECRET_LABEL, body))
}

/// Makes the credential for `claims`, signed with `key`, at `now` (milliseconds since the epoch,
/// for the lifetime that the ceiling of the key bounds). A server refuses claims that exceed the
/// ceiling, so this refuses them first.
///
/// # Errors
/// The [`TokenError`] of [`Claims::validate`], or [`TokenError::Exceeds`].
pub fn mint(key: &MintingKey, claims: &Claims, now: u64) -> Result<Credential, TokenError> {
    claims.validate()?;
    key.ceiling.check(claims, now)?;
    let mut bytes = claims.body(&key.id);
    let mac = derive(key, MAC_LABEL, &bytes);
    let secret_access_key = secret_of(key, &bytes);
    bytes.extend_from_slice(&mac[..MAC_LEN]);
    Ok(Credential {
        access_key_id: format!("{MARKER}{}", URL_SAFE_NO_PAD.encode(bytes)),
        secret_access_key,
    })
}

/// The credential for `claims` signed with `key` whatever the claims and the ceiling say, as a
/// holder of the key could make it: for the tests of what the server refuses even then.
#[cfg(test)]
pub(crate) fn forge(key: &MintingKey, claims: &Claims) -> Credential {
    let mut bytes = claims.body(&key.id);
    let mac = derive(key, MAC_LABEL, &bytes);
    let secret_access_key = secret_of(key, &bytes);
    bytes.extend_from_slice(&mac[..MAC_LEN]);
    Credential {
        access_key_id: format!("{MARKER}{}", URL_SAFE_NO_PAD.encode(bytes)),
        secret_access_key,
    }
}

/// A cursor over the bytes of an id.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let (head, tail) = self.0.split_at_checked(len)?;
        self.0 = tail;
        Some(head)
    }

    fn byte(&mut self) -> Option<u8> {
        self.take(1).map(|bytes| bytes[0])
    }
}

/// The claims of an id and the key id it names, with the bytes the MAC covers.
fn parse(id: &str) -> Option<(Claims, String, Vec<u8>, Vec<u8>)> {
    let bytes = URL_SAFE_NO_PAD.decode(id.strip_prefix(MARKER)?).ok()?;
    let covered_len = bytes.len().checked_sub(MAC_LEN)?;
    let (covered, mac) = bytes.split_at(covered_len);
    let mut reader = Reader(covered);
    let role = match reader.byte()? {
        0 => TokenRole::Client,
        1 => TokenRole::Gc,
        _ => return None,
    };
    let ops = Ops::from_bits(reader.byte()?)?;
    let key_len = usize::from(reader.byte()?);
    let key_id = std::str::from_utf8(reader.take(key_len)?).ok()?.to_owned();
    let not_after = u64::from_be_bytes(reader.take(8)?.try_into().ok()?);
    let prefix_len = u16::from_be_bytes(reader.take(2)?.try_into().ok()?);
    let prefix = std::str::from_utf8(reader.take(usize::from(prefix_len))?).ok()?;
    reader.0.is_empty().then_some(())?;
    let claims = Claims {
        role,
        ops,
        prefix: prefix.to_owned(),
        not_after,
    };
    Some((claims, key_id, covered.to_vec(), mac.to_vec()))
}

/// What a good token gives the server.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Verified {
    pub(crate) claims: Claims,
    /// The secret access key the signature of the request is checked with.
    pub(crate) secret: String,
    /// The most bytes of a request body, if the ceiling of the key limits it.
    pub(crate) max_body: Option<u64>,
}

/// Checks the access key id `id` of a request against the minting keys: it is a token, a key
/// with the id it names is among `keys`, its MAC is the one that key gives, and its claims are
/// ones a token may carry (the prefix is validated again here).
///
/// # Errors
/// [`TokenError::Malformed`], [`TokenError::UnknownKeyId`], [`TokenError::BadMac`], the error of
/// [`Claims::validate`], or [`TokenError::Exceeds`].
// A check of revocation (a deny list the front reads) is the next step after the ceiling, on the
// claims and the id: it is not built.
pub(crate) fn verify(keys: &[MintingKey], id: &str, now: u64) -> Result<Verified, TokenError> {
    let (claims, key_id, covered, mac) = parse(id).ok_or(TokenError::Malformed)?;
    let key = keys.iter().find(|key| key.id == key_id);
    let key = key.ok_or(TokenError::UnknownKeyId)?;
    let expected = derive(key, MAC_LABEL, &covered);
    if !same(&expected[..MAC_LEN], &mac) {
        return Err(TokenError::BadMac);
    }
    claims.validate()?;
    key.ceiling.check(&claims, now)?;
    let secret = secret_of(key, &covered);
    let max_body = key.ceiling.max_object_bytes;
    Ok(Verified {
        claims,
        secret,
        max_body,
    })
}

/// The flag whose files hold the minting keys.
const FLAG: &str = "--token-key-file";

/// The minting keys in the files of the flag `--token-key-file`, in the order of the flags.
///
/// # Errors
/// The first file that cannot be used, as [`MintingKey::from_file`] says.
pub(super) fn token_keys(paths: &[std::path::PathBuf]) -> Result<Vec<MintingKey>, String> {
    paths
        .iter()
        .map(|path| MintingKey::from_file(FLAG, path))
        .collect()
}

/// Refuses minting keys that do not all have an id of their own: a token names its key by id
/// alone.
///
/// # Errors
/// What is wrong, naming the id and the flag (never a secret).
pub(super) fn ensure_distinct_ids(keys: &[MintingKey]) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for key in keys {
        if !seen.insert(key.id()) {
            let id = key.id();
            return Err(format!(
                "the minting key id {id:?} is used twice among {FLAG}"
            ));
        }
    }
    Ok(())
}

/// Whether two byte strings are equal, in a time that does not depend on where they differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0_u8, |seen, (x, y)| seen | (x ^ y)) == 0
}

/// Whether an access key id looks like one of a token.
pub(crate) fn is_token_id(id: &str) -> bool {
    id.starts_with(MARKER)
}

pub(crate) const EXPIRED: Refusal = Refusal {
    code: "ExpiredToken",
    message: "The token has expired.",
};

const NOT_ALLOWED: Refusal = Refusal {
    code: "AccessDenied",
    message: "Access Denied: this token does not allow this call.",
};

const OUT_OF_SCOPE: Refusal = Refusal {
    code: "AccessDenied",
    message: "Access Denied: the key, or the listing prefix, is outside the prefix of this token.",
};

const NO_OVERWRITE: Refusal = Refusal {
    code: "AccessDenied",
    message: "Access Denied: this token creates objects and does not replace them.",
};

impl Claims {
    /// Whether the call may be done with this token.
    ///
    /// # Errors
    /// [`NOT_ALLOWED`] for a call the operations do not cover, [`OUT_OF_SCOPE`] for a key or a
    /// listing prefix outside the prefix, [`NO_OVERWRITE`] for a put that asks to replace.
    pub(super) fn permit(
        &self,
        parts: &Parts,
        target: &Target,
        query: &Query,
    ) -> Result<(), Refusal> {
        let multipart = query.has_any(&MULTIPART);
        let allow = |op: bool| if op { Ok(()) } else { Err(NOT_ALLOWED) };
        match (&parts.method, target, multipart) {
            (&Method::HEAD, Target::Bucket, false) => Ok(()),
            (&Method::GET, Target::Bucket, false) => {
                allow(self.ops.read)?;
                self.covers(query.get("prefix").unwrap_or_default())
            }
            (&Method::GET | &Method::HEAD, Target::Object(key), false) => {
                allow(self.ops.read)?;
                self.covers(key)
            }
            (&Method::PUT, Target::Object(key), _)
            | (&Method::POST | &Method::DELETE, Target::Object(key), true) => {
                allow(self.ops.create)?;
                if header(&parts.headers, "if-match").is_some() {
                    return Err(NO_OVERWRITE);
                }
                self.covers(key)
            }
            (&Method::DELETE, Target::Object(key), false) => {
                allow(self.ops.delete && self.role == TokenRole::Gc)?;
                self.covers(key)
            }
            _ => Err(NOT_ALLOWED),
        }
    }

    /// Whether `name`, a key or a listing prefix, is a valid name that starts with the prefix of
    /// the token and is not reserved.
    fn covers(&self, name: &str) -> Result<(), Refusal> {
        let inside = name.starts_with(&self.prefix) && !is_reserved(name);
        if inside && check_name(name).is_ok() {
            Ok(())
        } else {
            Err(OUT_OF_SCOPE)
        }
    }
}

#[cfg(test)]
#[path = "token_tests.rs"]
pub(super) mod tests;
