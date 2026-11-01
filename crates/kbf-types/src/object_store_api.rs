//! The object-store registration vocabulary (`docs/api.md`, `/v1/object-stores/...`): the
//! names a connection and a binding go by, the request bodies the operator sends and the
//! rows the audit log shows.
//!
//! **Where the secret lives.** [`Secret`] is the only type that holds a secret access key.
//! It zeroizes its bytes on drop, prints as `Secret(..)`, has no `Serialize` and no `Clone`
//! (a copy is a second buffer to wipe), and no `PartialEq` (an equality test of secrets is
//! the constant-time compare of the caller that holds both, not a derive). Every answer type
//! here has a field for whether a secret is set and none for the secret or a digest of it.

use std::fmt;
use std::num::NonZeroU64;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// The longest connection name, in characters.
const NAME_MAX: usize = 63;

/// A secret access key: wiped from memory on drop, redacted in `Debug`, never serialized.
///
/// The text is read only through [`Secret::expose`]. Serde builds one from a JSON string
/// ([`Deserialize`]) and cannot write one back.
pub struct Secret(Zeroizing<String>);

impl Secret {
    /// A secret made of `text`, which this takes over: the caller's copy is gone.
    #[must_use]
    pub fn new(text: String) -> Self {
        Self(Zeroizing::new(text))
    }

    /// The secret's text. The one way to read it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Secret::new)
    }
}

/// Why a connection name is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    /// Empty, or longer than 63 characters.
    #[error("a connection name is 1 to 63 characters")]
    Length,
    /// A character outside `a-z`, `0-9`, `_` and `-`, or a first character that is not a
    /// letter or digit. The name becomes part of a file name and of a URL path with a
    /// `:verify` custom method, so `/`, `.`, `:` and NUL are all refused here.
    #[error("a connection name starts with a-z or 0-9 and holds only a-z, 0-9, _ and -")]
    Character,
}

/// A connection's name: `[a-z0-9][a-z0-9_-]{0,62}`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnectionName(String);

impl ConnectionName {
    /// The name, as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ConnectionName {
    type Err = NameError;

    fn from_str(text: &str) -> Result<Self, NameError> {
        if text.is_empty() || text.len() > NAME_MAX {
            return Err(NameError::Length);
        }
        let first_ok = text
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
        let rest_ok = text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
        if first_ok && rest_ok {
            Ok(Self(text.to_owned()))
        } else {
            Err(NameError::Character)
        }
    }
}

impl TryFrom<String> for ConnectionName {
    type Error = NameError;

    fn try_from(text: String) -> Result<Self, NameError> {
        text.parse()
    }
}

impl From<ConnectionName> for String {
    fn from(name: ConnectionName) -> String {
        name.0
    }
}

impl fmt::Display for ConnectionName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for ConnectionName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ConnectionName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// What a binding is for. `cold` is today's `--cold-store`; the vocabulary has no other
/// role until something consumes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingRole {
    /// The cold store of the storage nodes.
    Cold,
}

/// Why a binding role is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RoleFault {
    /// `history` and `backup`: planned, with no consumer yet (`role_not_supported`).
    #[error("this role has no consumer yet; only cold is supported")]
    Unsupported,
    /// Any other text.
    #[error("unknown role; only cold is supported")]
    Unknown,
}

impl BindingRole {
    /// The role's name, as a URL and a row spell it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cold => "cold",
        }
    }
}

impl FromStr for BindingRole {
    type Err = RoleFault;

    fn from_str(text: &str) -> Result<Self, RoleFault> {
        match text {
            "cold" => Ok(Self::Cold),
            "history" | "backup" => Err(RoleFault::Unsupported),
            _ => Err(RoleFault::Unknown),
        }
    }
}

impl fmt::Display for BindingRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `PUT /v1/object-stores/connections/{name}`: what the operator sends. The secret is the
/// only secret in it; this type has no `Serialize`, so it cannot be echoed.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionPut {
    /// `http://host[:port]` (TLS to the store is not supported yet).
    pub endpoint: String,
    /// The signing region; the store's default when absent.
    #[serde(default)]
    pub region: Option<String>,
    /// The access key id (not secret).
    pub access_key_id: String,
    /// The secret access key.
    pub secret_access_key: Secret,
    /// Whether the store refuses to overwrite a key; `false` when absent.
    #[serde(default)]
    pub conditional_put: Option<bool>,
    /// The slowest request body rate, in bytes a second; the store default when absent.
    #[serde(default)]
    pub min_put_rate: Option<NonZeroU64>,
    /// The generation the operator last read: the write is refused if the registry has
    /// moved on.
    #[serde(default)]
    pub expect_generation: Option<u64>,
    /// Whether the frontend probes the store first; `true` when absent.
    #[serde(default)]
    pub verify: Option<bool>,
}

/// `PUT /v1/object-stores/bindings/{role}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingPut {
    /// The connection the role uses.
    pub connection: ConnectionName,
    /// The bucket.
    pub bucket: String,
    /// The key prefix in the bucket, empty for none.
    #[serde(default)]
    pub prefix: String,
    /// The generation the operator last read.
    #[serde(default)]
    pub expect_generation: Option<u64>,
    /// Whether the frontend probes the bucket first; `true` when absent.
    #[serde(default)]
    pub verify: Option<bool>,
}

/// `POST /v1/object-stores/connections/{name}:verify`: where to probe. The body may be
/// empty or `{}`, and then the probe uses the bucket and prefix of the `cold` binding that
/// names the connection.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifyRequest {
    /// The bucket to probe; the cold binding's when absent.
    #[serde(default)]
    pub bucket: Option<String>,
    /// The key prefix to probe under; the cold binding's (or empty) when absent.
    #[serde(default)]
    pub prefix: Option<String>,
}

/// What a probe of a store proved, step by step.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verified {
    /// All of the steps passed.
    pub ok: bool,
    /// A probe object was written.
    pub put: bool,
    /// It was read back.
    pub get: bool,
    /// It was deleted.
    pub delete: bool,
    /// A second write of the key was refused, as `conditional_put` promises.
    pub conditional_put: bool,
}

/// The answer to a connection PUT: no secret and no digest of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionPutAnswer {
    /// The connection.
    pub name: ConnectionName,
    /// The registry generation after the write (unchanged when `changed` is false).
    pub generation: u64,
    /// Whether the write changed anything.
    pub changed: bool,
    /// The access key id.
    pub access_key_id: String,
    /// Whether a secret is held for the connection.
    pub secret_set: bool,
    /// The probe, when one ran.
    pub verified: Option<Verified>,
}

/// A connection as the operator sees it: never the secret or a digest of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionInfo {
    /// Its name.
    pub name: ConnectionName,
    /// `http://host[:port]`.
    pub endpoint: String,
    /// The signing region; empty for the store's default.
    pub region: String,
    /// The access key id.
    pub access_key_id: String,
    /// Whether the store refuses to overwrite a key.
    pub conditional_put: bool,
    /// The slowest request body rate; absent for the store default.
    pub min_put_rate: Option<NonZeroU64>,
    /// The registry generation that last changed it.
    pub updated_generation: u64,
}

/// A binding as the operator sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingInfo {
    /// The role.
    pub role: BindingRole,
    /// The connection.
    pub connection: ConnectionName,
    /// The bucket.
    pub bucket: String,
    /// The key prefix.
    pub prefix: String,
    /// The registry generation that last changed it.
    pub updated_generation: u64,
}

/// What a mutation did, as the audit log names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditAction {
    /// `PUT /connections/{name}`.
    PutConnection,
    /// `DELETE /connections/{name}`.
    DeleteConnection,
    /// `PUT /bindings/{role}`.
    PutBinding,
    /// `DELETE /bindings/{role}`.
    DeleteBinding,
    /// `POST /connections/{name}:repoint`.
    Repoint,
    /// An action code this build does not know, written by a newer one.
    Unknown(u32),
}

/// How a mutation ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    /// The row did not exist.
    Created,
    /// The row changed.
    Changed,
    /// The same content: no generation bump.
    Unchanged,
    /// The row is gone.
    Deleted,
}

/// One audit row. It names what was touched and what happened, never a field of the body.
/// The actor is always the API token: the gate knows no finer identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Its place in the log, from 1.
    pub seq: u64,
    /// Store-clock seconds.
    pub at: u64,
    /// What was asked.
    pub action: AuditAction,
    /// The connection name or the role.
    pub subject: String,
    /// What happened.
    pub outcome: AuditOutcome,
    /// The registry generation after the call.
    pub generation: u64,
    /// What the call changed, when the action says more than its subject does (a
    /// repoint: the old and the new store identity). Absent for the other actions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// `POST .../connections/{name}:verify`: the probe, with nothing changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyAnswer {
    /// The connection probed.
    pub name: ConnectionName,
    /// The steps and whether they passed.
    pub verified: Verified,
}

/// The answer to a binding PUT.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingPutAnswer {
    /// The role.
    pub role: BindingRole,
    /// The registry generation after the write (unchanged when `changed` is false).
    pub generation: u64,
    /// Whether the write changed anything.
    pub changed: bool,
    /// The probe of the bucket, when one ran.
    pub verified: Option<Verified>,
}

/// Which store a binding names: the endpoint of its connection, its bucket and its prefix.
/// Nodes that run on the registration compare it with their own store, so a change of it
/// moves them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreIdentity {
    /// `http://host[:port]`.
    pub endpoint: String,
    /// The bucket.
    pub bucket: String,
    /// The key prefix; empty for none.
    pub prefix: String,
}

impl fmt::Display for StoreIdentity {
    /// `endpoint bucket/prefix`, as the audit log's detail spells it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}/{}", self.endpoint, self.bucket, self.prefix)
    }
}

/// `POST /v1/object-stores/connections/{name}:repoint`: move the store the `cold` binding
/// names, on purpose. There is no `verify` field: a repoint is always probed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepointRequest {
    /// The new endpoint, `http://host[:port]`.
    pub endpoint: String,
    /// The new bucket; the binding's current one when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
    /// The new key prefix; the binding's current one when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// The generation the operator last read: required, and the call is refused if the
    /// registry has moved on.
    pub expect_generation: u64,
}

/// The answer to a repoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepointAnswer {
    /// The connection.
    pub name: ConnectionName,
    /// The registry generation after the call (unchanged when `changed` is false).
    pub generation: u64,
    /// Whether the identity changed; a repoint to the current one changes nothing.
    pub changed: bool,
    /// The identity before the call.
    pub from: StoreIdentity,
    /// The identity after it.
    pub to: StoreIdentity,
    /// The probe of the new store.
    pub verified: Verified,
}

/// The answer to a DELETE.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deleted {
    /// The registry generation after the delete.
    pub generation: u64,
}

/// `GET /v1/object-stores/connections`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionList {
    /// Every connection, by name.
    pub connections: Vec<ConnectionInfo>,
}

/// `GET /v1/object-stores/audit`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditList {
    /// The newest rows first.
    pub entries: Vec<AuditEntry>,
}

/// What a storage node reports about the registration, as a closed set: never a node's
/// own text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    /// The node opened the store with the registration and probed it.
    Ok,
    /// The node has not applied the current generation (or has not reported since the
    /// frontend started).
    Pending,
    /// The node's probe of the store failed; it keeps the store it had.
    ProbeFailed,
    /// The node runs on a `--cold-store` flag whose identity differs from the
    /// registration; it keeps the flag's store.
    IdentityMismatch,
    /// The node's build does not report the registration at all (version skew).
    Unsupported,
}

/// A storage member's standing, for what convergence counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberStanding {
    /// Takes part: expected to apply the registration.
    Active,
    /// Leaving: not expected to apply it.
    Draining,
    /// On the deny list: not expected to apply it, and never handed it.
    Denied,
}

/// One storage node in the convergence view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeConvergence {
    /// The node id.
    pub id: String,
    /// Whether it is expected to apply.
    pub state: MemberStanding,
    /// The generation it last reported applying; absent if it never reported.
    pub applied_generation: Option<u64>,
    /// What it reports.
    pub status: NodeStatus,
    /// Seconds since its last heartbeat; absent if the frontend has not heard from it
    /// since it started.
    pub last_seen_secs: Option<u64>,
}

/// Whether the storage nodes run on the current registration. Never a convergence the
/// frontend does not know.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Convergence {
    /// Every expected node has applied the current generation with status `ok`, none is
    /// `unsupported`, and at least one node is expected.
    pub converged: bool,
    /// Active nodes that report the registration (draining, denied and `unsupported`
    /// nodes are not counted).
    pub expected: u64,
    /// Of those, the nodes at the current generation with status `ok`.
    pub applied: u64,
    /// Active nodes whose build does not report the registration.
    pub unsupported: u64,
    /// Every storage node, by id.
    pub nodes: Vec<NodeConvergence>,
}

/// `GET /v1/object-stores`: the desired state, and how far the nodes are from it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// The registry generation.
    pub generation: u64,
    /// Every connection.
    pub connections: Vec<ConnectionInfo>,
    /// Every binding.
    pub bindings: Vec<BindingInfo>,
    /// The nodes.
    pub convergence: Convergence,
}

impl Convergence {
    /// The view of `nodes` against `generation`: a node that reports `ok` at another
    /// generation is `pending`, and the counts and `converged` follow the field docs.
    #[must_use]
    pub fn of(generation: u64, mut nodes: Vec<NodeConvergence>) -> Self {
        nodes.sort_by(|a, b| a.id.cmp(&b.id));
        for node in &mut nodes {
            if node.status == NodeStatus::Ok && node.applied_generation != Some(generation) {
                node.status = NodeStatus::Pending;
            }
        }
        let active = |n: &&NodeConvergence| n.state == MemberStanding::Active;
        let unsupported = nodes
            .iter()
            .filter(active)
            .filter(|n| n.status == NodeStatus::Unsupported)
            .count() as u64;
        let expected = nodes.iter().filter(active).count() as u64 - unsupported;
        let applied = nodes
            .iter()
            .filter(active)
            .filter(|n| n.status == NodeStatus::Ok)
            .count() as u64;
        Self {
            converged: expected > 0 && applied == expected && unsupported == 0,
            expected,
            applied,
            unsupported,
            nodes,
        }
    }
}

/// An error answer: `error` is for people, `code` is closed and stable. Neither ever holds
/// the request body, a secret, or text a store or a node sent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    /// A sentence.
    pub error: String,
    /// The closed code (`docs/api.md`).
    pub code: String,
    /// The probe's steps, on `probe_failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified: Option<Verified>,
}

#[cfg(test)]
mod tests;
