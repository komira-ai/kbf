//! A SHA-256 digest written as text: 64 lowercase hex characters. It stands alone so that a
//! module which only needs to hold or check a digest (`installer.toml`'s CA pin) does not
//! import the order record.

use serde::{Deserialize, Serialize};

/// A SHA-256 digest: 64 lowercase hex characters.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Sha256Hex(String);

impl TryFrom<String> for Sha256Hex {
    type Error = String;

    fn try_from(s: String) -> Result<Self, String> {
        if s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            Ok(Sha256Hex(s))
        } else {
            Err(format!("{s:?} is not 64 lowercase hex characters"))
        }
    }
}

impl From<Sha256Hex> for String {
    fn from(h: Sha256Hex) -> String {
        h.0
    }
}

impl Sha256Hex {
    /// The digest as text: 64 lowercase hex characters.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
#[path = "sha256_hex_tests.rs"]
mod tests;
