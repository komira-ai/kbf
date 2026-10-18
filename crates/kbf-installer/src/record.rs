//! The order record: the one file per wire order that `kbf-update` writes into the installer's
//! `orders/` directory (docs/design/kbf-installer.md section 4).
//!
//! This module fixes the *shape*: the types, the strict parse and the syntax of each field. A
//! record is a locator and a set of claims, never trust, and nothing in it is a path, a URL, a
//! command or a unit name. The rules about *which* records act (the (epoch, generation) order,
//! floors, a live lease) are the installer's and come with `P6`; the golden fixtures in
//! `testdata/golden/` pin the shape for every later change.
//!
//! Unknown keys and duplicate keys are refused (serde's derived parser does both), a record
//! larger than [`MAX_RECORD_BYTES`] is refused before it is parsed, and the key set a verb
//! carries is checked: `activate` has components and the expected facts, `confirm` and
//! `revert` have neither.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::CommitId;

/// The largest record the installer reads.
pub const MAX_RECORD_BYTES: usize = 1 << 20;
/// The largest attestation bundle one component carries.
pub const MAX_ATTESTATION_BYTES: usize = 256 * 1024;
/// A platform needs at most this many components (the binary and, on some, a second one).
pub const MAX_COMPONENTS: usize = 4;

/// What a record asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrderVerb {
    Activate,
    Confirm,
    Revert,
}

/// The platforms a release is built for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TargetPlatform {
    #[serde(rename = "linux-x86_64")]
    LinuxX86_64,
    #[serde(rename = "linux-arm64")]
    LinuxArm64,
    #[serde(rename = "darwin-arm64")]
    DarwinArm64,
}

/// A SHA-256 digest: 64 lowercase hex characters.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Sha256Hex(String);

impl Sha256Hex {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

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

/// An activation: the order that started a swap, `<epoch>-<order id>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ActivationId {
    pub epoch: u64,
    pub order_id: u64,
}

impl FromStr for ActivationId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let bad = || format!("{s:?} is not an activation id (<epoch>-<order id>, decimal)");
        let (epoch, order) = s.split_once('-').ok_or_else(bad)?;
        let number = |t: &str| {
            if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
                return Err(bad());
            }
            t.parse::<u64>().map_err(|_| bad())
        };
        Ok(ActivationId {
            epoch: number(epoch)?,
            order_id: number(order)?,
        })
    }
}

impl fmt::Display for ActivationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.epoch, self.order_id)
    }
}

impl TryFrom<String> for ActivationId {
    type Error = String;

    fn try_from(s: String) -> Result<Self, String> {
        s.parse()
    }
}

impl From<ActivationId> for String {
    fn from(id: ActivationId) -> String {
        id.to_string()
    }
}

/// One file of the release for the host's platform, as the offer names it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Component {
    /// The file's name inside the tarball, `[a-z0-9._-]`, not starting with a dot.
    pub name: String,
    pub sha256: Sha256Hex,
    /// Bytes of the tarball.
    pub size: u64,
    /// The attestation bundle, JSON text, at most [`MAX_ATTESTATION_BYTES`].
    pub attestation: String,
}

/// What the frontend's preflight says the release declares; the installer checks the binary's
/// own `--version --json` against it before anything is renamed into place.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expected {
    pub platform: TargetPlatform,
    pub protocol: u32,
    pub min_protocol: u32,
    pub state_schema: u32,
    /// The build profile, `[a-z0-9_-]`, at most 32 characters.
    pub profile: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderRecord {
    pub order_id: u64,
    pub epoch: u64,
    pub generation: u64,
    pub verb: OrderVerb,
    pub commit: CommitId,
    /// The confirm window the frontend asks for; the installer clamps it.
    pub window_s: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<Component>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<Expected>,
}

/// Why bytes are not an order record.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum RecordError {
    #[error("record is {0} bytes; at most {MAX_RECORD_BYTES}")]
    TooLarge(usize),
    #[error("record is not valid: {0}")]
    Syntax(String),
    #[error("{0}")]
    Shape(String),
}

fn name_ok(s: &str, max: usize, dot_first: bool) -> bool {
    !s.is_empty()
        && s.len() <= max
        && (dot_first || !s.starts_with('.'))
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}

impl OrderRecord {
    /// Parse the record `bytes`, read once into memory by the caller.
    pub fn parse(bytes: &[u8]) -> Result<OrderRecord, RecordError> {
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(RecordError::TooLarge(bytes.len()));
        }
        let record: OrderRecord =
            serde_json::from_slice(bytes).map_err(|e| RecordError::Syntax(e.to_string()))?;
        record.check_shape().map_err(RecordError::Shape)?;
        Ok(record)
    }

    pub fn activation(&self) -> ActivationId {
        ActivationId {
            epoch: self.epoch,
            order_id: self.order_id,
        }
    }

    fn check_shape(&self) -> Result<(), String> {
        match self.verb {
            OrderVerb::Confirm | OrderVerb::Revert => {
                if !self.components.is_empty() || self.expected.is_some() {
                    return Err(format!(
                        "a {:?} record carries no components and no expected facts",
                        self.verb
                    ));
                }
                return Ok(());
            }
            OrderVerb::Activate => {}
        }
        if self.components.is_empty() || self.components.len() > MAX_COMPONENTS {
            return Err(format!(
                "an activate record carries 1 to {MAX_COMPONENTS} components, not {}",
                self.components.len()
            ));
        }
        let Some(expected) = &self.expected else {
            return Err("an activate record carries the expected facts".to_owned());
        };
        if !name_ok(&expected.profile, 32, false) || expected.profile.contains('.') {
            return Err(format!("profile {:?} is not [a-z0-9_-]", expected.profile));
        }
        for (i, c) in self.components.iter().enumerate() {
            if !name_ok(&c.name, 64, false) {
                return Err(format!(
                    "component name {:?} is not a plain file name",
                    c.name
                ));
            }
            if c.size == 0 {
                return Err(format!("component {:?} has size 0", c.name));
            }
            if c.attestation.is_empty() || c.attestation.len() > MAX_ATTESTATION_BYTES {
                return Err(format!(
                    "component {:?}: the attestation is 1 to {MAX_ATTESTATION_BYTES} bytes",
                    c.name
                ));
            }
            if self.components[..i].iter().any(|p| p.name == c.name) {
                return Err(format!("component {:?} is named twice", c.name));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "record_tests.rs"]
mod tests;
