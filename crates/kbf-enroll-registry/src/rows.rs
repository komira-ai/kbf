//! The rows, their keys and their codec.
//!
//! Every row is a hand-derived `prost` message, not a generated one, so that a binary
//! reading a row written by a newer binary (a handover runs both against one store) ignores
//! the fields it does not know instead of refusing the row. Numbers are never reused.

use kbf_state::{StoreTime, Version, Versioned};
use kbf_types::{NodeId, Role, RoleSet, TokenId};

use kbf_state::Store;

use crate::error::{Corrupt, Internal};

pub(crate) const TOKEN_PREFIX: &str = "enroll/token/";
pub(crate) const NODE_PREFIX: &str = "enroll/node/";
pub(crate) const DENY_PREFIX: &str = "deny/";
pub(crate) const CA_GENERATION: &str = "enroll/ca/generation";

pub(crate) fn token_key(id: &TokenId) -> String {
    format!("{TOKEN_PREFIX}{id}")
}
pub(crate) fn node_key(id: &NodeId) -> String {
    format!("{NODE_PREFIX}{id}")
}
pub(crate) fn serial_key(serial: &str) -> String {
    format!("enroll/serial/{serial}")
}
pub(crate) fn spki_key(spki: &[u8]) -> String {
    format!("enroll/key/{}", hex(spki))
}
pub(crate) fn advertise_key(host_port: &str) -> String {
    format!("enroll/adv/{host_port}")
}
pub(crate) fn request_key(token: &TokenId, csr_hash: &[u8; 32]) -> String {
    format!("enroll/req/{token}/{}", hex(csr_hash))
}
pub(crate) fn renew_key(node: &NodeId, csr_hash: &[u8; 32]) -> String {
    format!("enroll/renew/{node}/{}", hex(csr_hash))
}
pub(crate) fn deny_node_key(node: &NodeId) -> String {
    format!("{DENY_PREFIX}node/{node}")
}
pub(crate) fn deny_serial_key(serial: &str) -> String {
    format!("{DENY_PREFIX}serial/{serial}")
}
pub(crate) fn deny_spki_key(spki: &[u8]) -> String {
    format!("{DENY_PREFIX}spki-sha256/{}", hex(spki))
}

/// Lower-case hex, two digits a byte.
pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 15)]));
    }
    out
}

/// A serial's canonical spelling: lower-case hex of its big-endian bytes without leading
/// zeros, which is `kbf_map::normalize_serial`'s (and so the deny list's).
pub(crate) fn serial_text(bytes: &[u8]) -> String {
    kbf_map::normalize_serial(&hex(bytes))
}

/// A time as the seconds the rows hold. The store's clock, never the caller's.
pub(crate) fn seconds(t: StoreTime) -> u64 {
    t.as_duration().as_secs()
}

/// `enroll/token/<id>`.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct TokenRow {
    #[prost(bytes = "vec", tag = "1")]
    pub secret_sha256: Vec<u8>,
    #[prost(string, repeated, tag = "2")]
    pub roles: Vec<String>,
    #[prost(uint32, tag = "3")]
    pub max_uses: u32,
    #[prost(uint32, tag = "4")]
    pub uses: u32,
    #[prost(uint64, tag = "5")]
    pub expires_at: u64,
    #[prost(string, optional, tag = "6")]
    pub node_id: Option<String>,
    #[prost(string, optional, tag = "7")]
    pub node_prefix: Option<String>,
    #[prost(string, repeated, tag = "8")]
    pub names: Vec<String>,
    #[prost(bool, tag = "9")]
    pub replace: bool,
    #[prost(string, tag = "10")]
    pub label: String,
    #[prost(uint64, tag = "11")]
    pub created_at: u64,
    #[prost(string, tag = "12")]
    pub created_by: String,
    #[prost(uint64, optional, tag = "13")]
    pub revoked_at: Option<u64>,
}

/// One certificate a node holds or held.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct CertRow {
    #[prost(string, tag = "1")]
    pub role: String,
    #[prost(bytes = "vec", tag = "2")]
    pub spki_sha256: Vec<u8>,
    #[prost(string, tag = "3")]
    pub serial: String,
    #[prost(uint64, tag = "4")]
    pub not_before: u64,
    #[prost(uint64, tag = "5")]
    pub not_after: u64,
    #[prost(bytes = "vec", tag = "6")]
    pub cert_der: Vec<u8>,
}

/// An earlier certificate of a node that has not expired yet: still a certificate to deny.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct PriorRow {
    #[prost(string, tag = "1")]
    pub role: String,
    #[prost(string, tag = "2")]
    pub serial: String,
    #[prost(uint64, tag = "3")]
    pub not_after: u64,
}

pub(crate) const STATE_ACTIVE: i32 = 1;
pub(crate) const STATE_REVOKED: i32 = 2;

/// `enroll/node/<id>`.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct NodeRow {
    #[prost(message, repeated, tag = "1")]
    pub certs: Vec<CertRow>,
    #[prost(string, tag = "2")]
    pub advertise: String,
    #[prost(string, tag = "3")]
    pub zone: String,
    #[prost(string, tag = "4")]
    pub token_id: String,
    #[prost(string, tag = "5")]
    pub token_label: String,
    #[prost(int32, tag = "6")]
    pub state: i32,
    #[prost(uint64, tag = "7")]
    pub created_at: u64,
    #[prost(uint64, optional, tag = "8")]
    pub last_renewed_at: Option<u64>,
    #[prost(uint64, optional, tag = "9")]
    pub revoked_at: Option<u64>,
    #[prost(string, tag = "10")]
    pub revoked_by: String,
    #[prost(message, repeated, tag = "11")]
    pub priors: Vec<PriorRow>,
}

/// `enroll/serial/<hex>`.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct SerialRow {
    #[prost(string, tag = "1")]
    pub node_id: String,
    #[prost(string, tag = "2")]
    pub role: String,
    #[prost(uint64, tag = "3")]
    pub not_after: u64,
    #[prost(uint64, optional, tag = "4")]
    pub superseded_at: Option<u64>,
}

/// `enroll/key/<spki sha256>`, `enroll/req/...` and `enroll/adv/...`: the node they name.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct OwnerRow {
    #[prost(string, tag = "1")]
    pub node_id: String,
    #[prost(string, tag = "2")]
    pub role: String,
}

/// `deny/<kind>/<value>`.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct DenyRow {
    #[prost(string, tag = "1")]
    pub reason: String,
    #[prost(uint64, tag = "2")]
    pub at: u64,
    #[prost(string, tag = "3")]
    pub by: String,
}

/// Decodes `stored` as an `M`; `family` names the row family in the error.
pub(crate) fn decode<M: prost::Message + Default>(
    stored: &Versioned,
    family: &'static str,
) -> Result<M, Corrupt> {
    M::decode(stored.value.as_slice()).map_err(|_| Corrupt(family))
}

/// A decoded row and the version it was read at.
pub(crate) struct Read<M> {
    pub row: M,
    pub version: Version,
}

/// The live row at `key`, decoded, with the version it was read at.
pub(crate) fn fetch<M: prost::Message + Default>(
    store: &dyn Store,
    key: &str,
    family: &'static str,
) -> Result<Option<Read<M>>, Internal> {
    store
        .get(key)?
        .map(|stored| {
            Ok(Read {
                row: decode(&stored, family)?,
                version: stored.version,
            })
        })
        .transpose()
}

/// The roles a row names, as a set.
pub(crate) fn role_set(names: &[String], family: &'static str) -> Result<RoleSet, Corrupt> {
    RoleSet::from_names(names.iter().map(String::as_str)).map_err(|_| Corrupt(family))
}

pub(crate) fn role_of(name: &str, family: &'static str) -> Result<Role, Corrupt> {
    name.parse().map_err(|_| Corrupt(family))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Catches a serial spelt with a leading zero or in capitals: the deny list compares
    // exact strings, so a serial stored in any other spelling would never match.
    #[test]
    fn serial_text_is_the_deny_lists_spelling() {
        assert_eq!(serial_text(&[0x00, 0x0a, 0xff]), "aff");
        assert_eq!(serial_text(&[0x00, 0x00]), "0");
        assert_eq!(serial_text(&[0xAB, 0x01]), "ab01");
        assert_eq!(
            serial_text(&[0x00, 0x0a]),
            kbf_map::normalize_serial("000A")
        );
    }
}
