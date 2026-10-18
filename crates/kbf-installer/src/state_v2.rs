//! `state.json`, schema 2 (docs/design/kbf-installer.md section 3): the installer's record of
//! what is installed and what it is in the middle of. This is the *shape*, fixed by the golden
//! fixture `testdata/golden/state.json`; reading and writing it under the lock, the `.bak`
//! copy and the migration from schema 1 come with `P2` and `P3`.
//!
//! `floor` replaces schema 1's unused `order_floor`. `previous` holds confirmed releases only.
//! `unacked` is a copy of the order being acted on, with root's own first-seen time of the
//! record, so a restart reruns it and never lets the writer's file times decide its age.

use serde::{Deserialize, Serialize};

use crate::CommitId;
use crate::record::{ActivationId, OrderRecord};

pub const SCHEMA: u64 = 2;

/// The directory name of an installed Envoy: `<version>-<first 16 hex of its sha256>`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EnvoyId(String);

impl EnvoyId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for EnvoyId {
    type Error = String;

    fn try_from(s: String) -> Result<Self, String> {
        let ok = s.rsplit_once('-').is_some_and(|(version, sum)| {
            !version.is_empty()
                && version.len() <= 32
                && version
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_')
                && sum.len() == 16
                && sum.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        });
        if ok {
            Ok(EnvoyId(s))
        } else {
            Err(format!("{s:?} is not <version>-<16 hex>"))
        }
    }
}

impl From<EnvoyId> for String {
    fn from(id: EnvoyId) -> String {
        id.0
    }
}

/// A frontend instance name (`a`, `b`): `[a-z0-9]`, at most 16 characters.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Instance(String);

impl Instance {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Instance {
    type Error = String;

    fn try_from(s: String) -> Result<Self, String> {
        if (1..=16).contains(&s.len())
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        {
            Ok(Instance(s))
        } else {
            Err(format!("{s:?} is not an instance name ([a-z0-9], 1 to 16)"))
        }
    }
}

impl From<Instance> for String {
    fn from(i: Instance) -> String {
        i.0
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvoyPins {
    pub current: Option<EnvoyId>,
    pub previous: Option<EnvoyId>,
}

/// The highest order acted on: (epoch, order id) and the generation within the epoch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Floor {
    pub epoch: u64,
    pub order_id: u64,
    pub generation: u64,
}

/// A swap on trial.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingConfirm {
    pub activation: ActivationId,
    pub commit: CommitId,
    pub previous: Option<CommitId>,
    /// Seconds since the Unix epoch; `None` until the window starts.
    pub deadline_wall: Option<u64>,
    /// Seconds on the monotonic clock since boot, kept beside the wall deadline.
    pub deadline_mono_s: Option<u64>,
    /// Starts left before the release is reverted (Linux).
    pub starts_left: Option<u32>,
}

/// The order being acted on, kept until the flip acknowledges it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Unacked {
    pub order: OrderRecord,
    /// Seconds since the Unix epoch when root first saw the record.
    pub first_seen: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateV2 {
    pub schema: u64,
    pub current: Option<CommitId>,
    /// Confirmed releases only.
    pub previous: Option<CommitId>,
    pub staged: Option<CommitId>,
    pub pending_confirm: Option<PendingConfirm>,
    pub envoy: EnvoyPins,
    pub floor: Floor,
    /// The frontend instance that serves (frontend hosts).
    pub serving_instance: Option<Instance>,
    pub last_acked_order_id: u64,
    pub unacked: Option<Unacked>,
}

/// Why bytes are not a schema 2 state.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum StateError {
    #[error("state.json is not valid: {0}")]
    Syntax(String),
    #[error("state.json is schema {0}, this build reads schema 2")]
    Schema(u64),
}

impl StateV2 {
    /// A host that has installed nothing.
    pub fn new() -> StateV2 {
        StateV2 {
            schema: SCHEMA,
            current: None,
            previous: None,
            staged: None,
            pending_confirm: None,
            envoy: EnvoyPins::default(),
            floor: Floor::default(),
            serving_instance: None,
            last_acked_order_id: 0,
            unacked: None,
        }
    }

    pub fn parse(bytes: &[u8]) -> Result<StateV2, StateError> {
        let state: StateV2 =
            serde_json::from_slice(bytes).map_err(|e| StateError::Syntax(e.to_string()))?;
        if state.schema != SCHEMA {
            return Err(StateError::Schema(state.schema));
        }
        Ok(state)
    }

    /// The bytes as written: indented, a newline last.
    pub fn render(&self) -> Result<Vec<u8>, serde_json::Error> {
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

impl Default for StateV2 {
    fn default() -> Self {
        StateV2::new()
    }
}

#[cfg(test)]
#[path = "state_v2_tests.rs"]
mod tests;
