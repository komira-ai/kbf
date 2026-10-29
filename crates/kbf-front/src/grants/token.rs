//! The read grant's token: what it says, how it is minted, and how it is opened.

use std::fmt;

use kbf_auth::AuthenticationMetadata;
use kbf_types::Digest;

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
        Ok(Self(secret.to_vec()))
    }
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
        let _ = metadata;
        None
    }
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
        write!(f, "Token({})", self.0)
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

/// Mints the token for `claims` under `key`.
///
/// # Errors
/// The build id is not valid.
pub(super) fn mint(key: &GrantKey, claims: &Claims) -> Result<Token, ClaimsError> {
    let _ = sign(key, &claims.build);
    Ok(Token("stub".to_owned()))
}

/// The MAC of `body` under `key`, in lowercase hex.
pub(super) fn sign(key: &GrantKey, body: &str) -> String {
    let _ = (key, body);
    String::new()
}

/// The claims of `text` if its MAC matches under `key` (expiry and revocation are the
/// caller's).
///
/// # Errors
/// [`GrantError::Malformed`] or [`GrantError::BadMac`].
pub(super) fn open(key: &GrantKey, text: &str) -> Result<Claims, GrantError> {
    let _ = (key, text);
    Err(GrantError::Malformed)
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
