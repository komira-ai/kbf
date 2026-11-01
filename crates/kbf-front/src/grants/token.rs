//! The read grant's token: what it says, how it is minted, and how it is opened.

use std::fmt;

use hmac::{Hmac, KeyInit, Mac};
use kbf_auth::AuthenticationMetadata;
use kbf_types::{Digest, DigestFunction};
use serde_json::{Value, json};
use sha2::Sha256;

/// What the key derivation and the token's MAC are separated by from any other use of the
/// farm secret.
const DOMAIN: &[u8] = b"kbf-read-grant/v1";

/// The token text's version, its first field.
const VERSION: &str = "v1";

/// The longest build id a token carries.
const MAX_BUILD_LEN: usize = 64;

/// The bytes of a SHA-256 MAC, and so half the characters of its hex.
const MAC_LEN: usize = 32;

/// The shortest secret a [`GrantKey`] is made from, in bytes.
pub const MIN_SECRET_LEN: usize = 32;

/// The farm secret a grant's MAC is keyed from. Never printed.
pub struct GrantKey(Vec<u8>);

/// Why a secret is refused as a [`GrantKey`].
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    /// The secret is shorter than [`MIN_SECRET_LEN`].
    #[error("the grant secret has {got} bytes, at least {MIN_SECRET_LEN} are needed")]
    TooShort {
        /// Its length.
        got: usize,
    },
}

impl GrantKey {
    /// The grant key derived from the farm secret `secret`.
    ///
    /// # Errors
    /// The secret is shorter than [`MIN_SECRET_LEN`].
    pub fn from_secret(secret: &[u8]) -> Result<Self, KeyError> {
        if secret.len() < MIN_SECRET_LEN {
            return Err(KeyError::TooShort { got: secret.len() });
        }
        let mut derive = keyed(secret);
        derive.update(DOMAIN);
        Ok(Self(derive.finalize().into_bytes().to_vec()))
    }
}

fn keyed(key: &[u8]) -> Hmac<Sha256> {
    <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC accepts a key of any length")
}

impl fmt::Debug for GrantKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GrantKey(<redacted>)")
    }
}

/// What a grant says: whose build, for which Action, until when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claims {
    /// The lease id the grant was issued for.
    pub build: String,
    /// The Action the grant reads for.
    pub action: Digest,
    /// Unix seconds at which the grant stops being valid (it is refused at this second).
    pub not_after: u64,
}

impl Claims {
    /// The claims an authenticator put in `metadata`, if it did.
    #[must_use]
    pub fn of(metadata: &AuthenticationMetadata) -> Option<Self> {
        let grant = metadata.private()?.get("grant")?;
        Some(Self {
            build: grant.get("build")?.as_str()?.to_owned(),
            action: Digest::parse(DigestFunction::Sha256, grant.get("action")?.as_str()?).ok()?,
            not_after: grant.get("not_after")?.as_u64()?,
        })
    }

    /// The claims as the authenticator carries them in the private metadata.
    pub(super) fn to_json(&self) -> Value {
        json!({
            "build": self.build,
            "action": self.action.to_string(),
            "not_after": self.not_after,
        })
    }
}

fn valid_build(build: &str) -> bool {
    (1..=MAX_BUILD_LEN).contains(&build.len())
        && build
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Why claims cannot be minted.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ClaimsError {
    /// The build id is empty, too long or has a character outside `[0-9A-Za-z_-]`.
    #[error("the build id {0:?} is not 1 to 64 characters of [0-9A-Za-z_-]")]
    Build(String),
}

/// A minted token: opaque, secret, never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// The token's text, for the one place that hands it to the task.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// Why a token is refused.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum GrantError {
    /// The text is not a token of this version.
    #[error("the read grant is malformed")]
    Malformed,
    /// The MAC does not match: another key, or a changed token.
    #[error("the read grant is not valid")]
    BadMac,
    /// The grant's `not_after` has come.
    #[error("the read grant has expired")]
    Expired,
    /// The build's grants were revoked.
    #[error("the read grant was revoked")]
    Revoked,
}

/// Mints the token for `claims` under `key`: `v1.<build>.<action>.<not_after>.<MAC>`, the
/// MAC over everything before its dot.
///
/// # Errors
/// The build id is not valid.
pub(super) fn mint(key: &GrantKey, claims: &Claims) -> Result<Token, ClaimsError> {
    if !valid_build(&claims.build) {
        return Err(ClaimsError::Build(claims.build.clone()));
    }
    let body = format!(
        "{VERSION}.{}.{}.{}",
        claims.build, claims.action, claims.not_after
    );
    let mac = sign(key, &body);
    Ok(Token(format!("{body}.{mac}")))
}

/// The MAC of `body` under `key`, in lowercase hex.
pub(super) fn sign(key: &GrantKey, body: &str) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut mac = keyed(&key.0);
    mac.update(body.as_bytes());
    let mut out = String::with_capacity(MAC_LEN * 2);
    for byte in mac.finalize().into_bytes().to_vec() {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

/// The claims of `text` if its MAC matches under `key` (expiry and revocation are the
/// caller's). The MAC is checked first, in constant time, and nothing of the other fields
/// is read before it matches.
///
/// # Errors
/// [`GrantError::Malformed`] or [`GrantError::BadMac`].
pub(super) fn open(key: &GrantKey, text: &str) -> Result<Claims, GrantError> {
    let (body, mac) = text.rsplit_once('.').ok_or(GrantError::Malformed)?;
    let tag = unhex(mac).ok_or(GrantError::Malformed)?;
    let mut check = keyed(&key.0);
    check.update(body.as_bytes());
    check.verify_slice(&tag).map_err(|_| GrantError::BadMac)?;
    parse(body).ok_or(GrantError::Malformed)
}

/// The fields of a body whose MAC matched.
fn parse(body: &str) -> Option<Claims> {
    let mut fields = body.split('.');
    let (Some(VERSION), Some(build), Some(action), Some(not_after), None) = (
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
    ) else {
        return None;
    };
    if !valid_build(build) || !not_after.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(Claims {
        build: build.to_owned(),
        action: Digest::parse(DigestFunction::Sha256, action).ok()?,
        not_after: not_after.parse().ok()?,
    })
}

/// The bytes of a MAC written as lowercase hex; `None` for any other text.
fn unhex(text: &str) -> Option<Vec<u8>> {
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    if text.len() != MAC_LEN * 2 {
        return None;
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| Some((nibble(pair[0])? << 4) | nibble(pair[1])?))
        .collect()
}

#[cfg(test)]
mod tests {
    use kbf_types::DigestFunction;

    use super::*;

    fn key() -> GrantKey {
        GrantKey::from_secret(&[3; MIN_SECRET_LEN]).expect("a long enough secret")
    }

    /// A token text whose MAC is valid for `body`, whatever the body says: what only
    /// the holder of the key can write.
    fn forged(body: &str) -> String {
        format!("{body}.{}", sign(&key(), body))
    }

    const ACTION: &str = "0101010101010101010101010101010101010101010101010101010101010101/140";

    /// Catches: a field of a validly signed text read loosely (another version, a
    /// missing or extra field, a bad build id, a digest that does not parse, a time
    /// that is not a plain number), so a text the minter never writes opens.
    #[test]
    fn a_signed_text_that_is_not_a_grant_is_malformed() {
        let ok = format!("v1.b-1.{ACTION}.77");
        let claims = open(&key(), &forged(&ok)).expect("a well-formed body");
        assert_eq!(
            claims,
            Claims {
                build: "b-1".to_owned(),
                action: Digest::parse(DigestFunction::Sha256, ACTION).expect("a digest"),
                not_after: 77
            }
        );
        let bodies = [
            format!("v2.b-1.{ACTION}.77"),
            format!("v1.b-1.{ACTION}"),
            format!("v1.b-1.{ACTION}.77.extra"),
            format!("v1..{ACTION}.77"),
            format!("v1.b/1.{ACTION}.77"),
            "v1.b-1.nothex/140.77".to_owned(),
            format!("v1.b-1.{ACTION}.-1"),
            format!("v1.b-1.{ACTION}.99999999999999999999"),
            format!("v1.b-1.{ACTION}."),
        ];
        for body in bodies {
            assert_eq!(
                open(&key(), &forged(&body)),
                Err(GrantError::Malformed),
                "{body}"
            );
        }
    }
}
