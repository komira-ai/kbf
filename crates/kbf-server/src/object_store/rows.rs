//! The rows, their keys and their codec.
//!
//! Every row is a hand-derived `prost` message, not a generated one, so that a binary
//! reading a row written by a newer binary (a roll runs both against one store)
//! ignores the fields it does not know instead of refusing the row. Numbers are never
//! reused. There is no field for a secret or a digest of one in any row.

use kbf_state::Versioned;
use kbf_types::object_store_api::{AuditAction, AuditOutcome, BindingRole, ConnectionName};

use super::RegistryError;

pub(super) const CONN_PREFIX: &str = "objstore/conn/";
pub(super) const BIND_PREFIX: &str = "objstore/bind/";
pub(super) const AUDIT_PREFIX: &str = "objstore/audit/";
pub(super) const GEN_KEY: &str = "objstore/gen";

pub(super) fn conn_key(name: &ConnectionName) -> String {
    format!("{CONN_PREFIX}{name}")
}

pub(super) fn bind_key(role: BindingRole) -> String {
    format!("{BIND_PREFIX}{role}")
}

/// Zero-padded so the key order is the sequence order.
pub(super) fn audit_key(seq: u64) -> String {
    format!("{AUDIT_PREFIX}{seq:020}")
}

/// `objstore/conn/<name>`.
#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct ConnRow {
    #[prost(string, tag = "1")]
    pub endpoint: String,
    #[prost(string, tag = "2")]
    pub region: String,
    #[prost(string, tag = "3")]
    pub access_key_id: String,
    #[prost(bool, tag = "4")]
    pub conditional_put: bool,
    /// Bytes a second; 0 is the store default.
    #[prost(uint64, tag = "5")]
    pub min_put_rate: u64,
    /// Names the secret file.
    #[prost(uint64, tag = "6")]
    pub secret_version: u64,
    #[prost(uint64, tag = "7")]
    pub updated_generation: u64,
}

/// `objstore/bind/<role>`.
#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct BindRow {
    #[prost(string, tag = "1")]
    pub connection: String,
    #[prost(string, tag = "2")]
    pub bucket: String,
    #[prost(string, tag = "3")]
    pub prefix: String,
    #[prost(uint64, tag = "4")]
    pub updated_generation: u64,
}

/// `objstore/gen`: absent is generation 0 with no audit rows.
#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct GenRow {
    #[prost(uint64, tag = "1")]
    pub generation: u64,
    /// The sequence of the newest audit row.
    #[prost(uint64, tag = "2")]
    pub audit_seq: u64,
}

/// `objstore/audit/<seq>`: what was touched and what happened; no field of the body.
#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct AuditRow {
    #[prost(uint64, tag = "1")]
    pub at: u64,
    #[prost(int32, tag = "2")]
    pub action: i32,
    #[prost(string, tag = "3")]
    pub subject: String,
    #[prost(int32, tag = "4")]
    pub outcome: i32,
    #[prost(uint64, tag = "5")]
    pub generation: u64,
    /// What the action changed, when its subject does not say (a repoint); empty if not.
    #[prost(string, tag = "6")]
    pub detail: String,
}

pub(super) fn action_code(action: AuditAction) -> i32 {
    match action {
        AuditAction::PutConnection => 1,
        AuditAction::DeleteConnection => 2,
        AuditAction::PutBinding => 3,
        AuditAction::DeleteBinding => 4,
        AuditAction::Repoint => 5,
    }
}

pub(super) fn action_of(code: i32) -> Result<AuditAction, RegistryError> {
    Ok(match code {
        1 => AuditAction::PutConnection,
        2 => AuditAction::DeleteConnection,
        3 => AuditAction::PutBinding,
        4 => AuditAction::DeleteBinding,
        5 => AuditAction::Repoint,
        _ => return Err(RegistryError::Corrupt("audit")),
    })
}

pub(super) fn outcome_code(outcome: AuditOutcome) -> i32 {
    match outcome {
        AuditOutcome::Created => 1,
        AuditOutcome::Changed => 2,
        AuditOutcome::Unchanged => 3,
        AuditOutcome::Deleted => 4,
    }
}

pub(super) fn outcome_of(code: i32) -> Result<AuditOutcome, RegistryError> {
    Ok(match code {
        1 => AuditOutcome::Created,
        2 => AuditOutcome::Changed,
        3 => AuditOutcome::Unchanged,
        4 => AuditOutcome::Deleted,
        _ => return Err(RegistryError::Corrupt("audit")),
    })
}

/// Decodes `stored` as an `M`; `family` names the row family in the error.
pub(super) fn decode<M: prost::Message + Default>(
    stored: &Versioned,
    family: &'static str,
) -> Result<M, RegistryError> {
    M::decode(stored.value.as_slice()).map_err(|_| RegistryError::Corrupt(family))
}
