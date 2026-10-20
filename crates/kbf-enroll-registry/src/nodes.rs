//! The node registry: listing, revoking, and the deny rows revocation writes.

use kbf_map::DenyEntry;
use kbf_state::{Cond, Store, Txn};
use kbf_types::{NodeId, RoleSet};
use prost::Message as _;

use crate::ATTEMPTS;
use crate::error::{Corrupt, RegistryError};
use crate::rows::{self, DENY_PREFIX, DenyRow, NODE_PREFIX, NodeRow, STATE_ACTIVE, STATE_REVOKED};

/// Whether a node may renew and enroll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeState {
    /// Enrolled and in good standing.
    Active,
    /// Revoked: its id is burned, nothing renews it, and its certificates are denied.
    Revoked,
}

/// A registry row as the operator sees it. Certificates and keys are not in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeInfo {
    /// The node's id.
    pub id: NodeId,
    /// The roles it holds.
    pub roles: RoleSet,
    /// Its state.
    pub state: NodeState,
    /// The `host:port` it advertises; empty for a node with no storage role.
    pub advertise: String,
    /// Its zone label.
    pub zone: String,
    /// The token it enrolled with.
    pub token_id: String,
    /// That token's label when it enrolled.
    pub token_label: String,
    /// Store-clock seconds of its enrollment.
    pub created_at: u64,
    /// Store-clock seconds of its last renewal.
    pub last_renewed_at: Option<u64>,
    /// When its newest certificate expires.
    pub not_after: u64,
    /// Store-clock seconds of its revocation.
    pub revoked_at: Option<u64>,
}

pub(crate) fn state_of(row: &NodeRow) -> Result<NodeState, Corrupt> {
    match row.state {
        STATE_ACTIVE => Ok(NodeState::Active),
        STATE_REVOKED => Ok(NodeState::Revoked),
        _ => Err(Corrupt("node state")),
    }
}

fn node_info(id: NodeId, row: &NodeRow) -> Result<NodeInfo, Corrupt> {
    let names: Vec<String> = row.certs.iter().map(|c| c.role.clone()).collect();
    Ok(NodeInfo {
        id,
        roles: rows::role_set(&names, "node")?,
        state: state_of(row)?,
        advertise: row.advertise.clone(),
        zone: row.zone.clone(),
        token_id: row.token_id.clone(),
        token_label: row.token_label.clone(),
        created_at: row.created_at,
        last_renewed_at: row.last_renewed_at,
        not_after: row.certs.iter().map(|c| c.not_after).max().unwrap_or(0),
        revoked_at: row.revoked_at,
    })
}

/// Every node, by id, revoked ones included.
///
/// # Errors
/// The store failing; a row that does not decode.
pub fn list_nodes(store: &dyn Store) -> Result<Vec<NodeInfo>, RegistryError> {
    let mut out = Vec::new();
    for (key, stored) in store.scan_prefix(NODE_PREFIX)? {
        let id = key
            .strip_prefix(NODE_PREFIX)
            .and_then(|text| text.parse::<NodeId>().ok())
            .ok_or(Corrupt("node key"))?;
        out.push(node_info(id, &rows::decode(&stored, "node")?)?);
    }
    Ok(out)
}

/// One node's state: `None` for an id the registry has never seen. A single row read, for
/// a caller that asks at every call (the cold-store registration is handed out only to a
/// node that is `Active`).
///
/// # Errors
/// The store failing; a row that does not decode.
pub fn node_state(store: &dyn Store, id: &NodeId) -> Result<Option<NodeState>, RegistryError> {
    let read = rows::fetch::<NodeRow>(store, &rows::node_key(id), "node")?;
    Ok(read.map(|read| state_of(&read.row)).transpose()?)
}

/// The `deny/` keys that end a node's certificates: its unexpired serials (current and
/// earlier), the hash of each key, and with `with_node` the node id itself.
pub(crate) fn deny_keys(id: &NodeId, row: &NodeRow, now: u64, with_node: bool) -> Vec<String> {
    let mut keys = Vec::new();
    if with_node {
        keys.push(rows::deny_node_key(id));
    }
    let serials = row
        .certs
        .iter()
        .map(|c| (&c.serial, c.not_after))
        .chain(row.priors.iter().map(|p| (&p.serial, p.not_after)));
    keys.extend(
        serials
            .filter(|(_, not_after)| *not_after > now)
            .map(|(serial, _)| rows::deny_serial_key(serial)),
    );
    for cert in &row.certs {
        keys.push(rows::deny_spki_key(&cert.spki_sha256));
    }
    keys
}

/// Adds a deny row for each of `keys` to `txn`.
pub(crate) fn with_deny_rows(txn: Txn, keys: &[String], now: u64, by: &str, reason: &str) -> Txn {
    let value = DenyRow {
        reason: reason.to_owned(),
        at: now,
        by: by.to_owned(),
    }
    .encode_to_vec();
    keys.iter()
        .fold(txn, |txn, key| txn.put(key, value.clone(), Cond::Any))
}

/// The entry a `deny/<kind>/<value>` key stands for, in `kbf_map::DenyEntry`'s canonical
/// spelling.
pub(crate) fn entry_of(key: &str) -> Result<DenyEntry, Corrupt> {
    key.strip_prefix(DENY_PREFIX)
        .and_then(|rest| rest.split_once('/'))
        .and_then(|(kind, value)| format!("{kind} {value}").parse().ok())
        .ok_or(Corrupt("deny key"))
}

/// Revokes a node in one transaction: its row becomes `Revoked` (its id is burned), and
/// deny rows are written for the node id, every unexpired serial it holds or held and the
/// hash of each of its keys. Returns the entries written.
///
/// # Errors
/// [`RegistryError::NotFound`] for an unknown node; [`RegistryError::Invalid`] for a node
/// already revoked; the store failing; [`RegistryError::Contended`].
pub fn revoke_node(
    store: &dyn Store,
    id: &NodeId,
    by: &str,
) -> Result<Vec<DenyEntry>, RegistryError> {
    let key = rows::node_key(id);
    for _ in 0..ATTEMPTS {
        let read = rows::fetch::<NodeRow>(store, &key, "node")?.ok_or(RegistryError::NotFound)?;
        if state_of(&read.row)? == NodeState::Revoked {
            return Err(RegistryError::Invalid("the node is already revoked"));
        }
        let now = rows::seconds(store.now());
        let mut row = read.row;
        let keys = deny_keys(id, &row, now, true);
        row.state = STATE_REVOKED;
        row.revoked_at = Some(now);
        row.revoked_by = by.to_owned();
        let mut txn = Txn::new().put(&key, row.encode_to_vec(), Cond::Version(read.version));
        // The id stays burned, the address does not: another node may take it.
        if !row.advertise.is_empty() {
            txn = txn.delete(rows::advertise_key(&row.advertise), Cond::Any);
        }
        let txn = with_deny_rows(txn, &keys, now, by, "revoked");
        if store.txn(txn)?.is_ok() {
            return keys.iter().map(|k| Ok(entry_of(k)?)).collect();
        }
    }
    Err(RegistryError::Contended)
}

/// A deny row with who wrote it and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DenyRecord {
    /// What is denied.
    pub entry: DenyEntry,
    /// Why: `revoked` or `replaced`.
    pub reason: String,
    /// Store-clock seconds.
    pub at: u64,
    /// Who.
    pub by: String,
}

/// Every `deny/` row.
///
/// # Errors
/// The store failing; a row or key that does not decode.
pub fn deny_rows(store: &dyn Store) -> Result<Vec<DenyRecord>, RegistryError> {
    let mut out = Vec::new();
    for (key, stored) in store.scan_prefix(DENY_PREFIX)? {
        let row: DenyRow = rows::decode(&stored, "deny")?;
        out.push(DenyRecord {
            entry: entry_of(&key)?,
            reason: row.reason,
            at: row.at,
            by: row.by,
        });
    }
    Ok(out)
}
