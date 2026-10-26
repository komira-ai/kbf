//! The node registry: listing, revoking, and the deny rows revocation writes.

use kbf_map::DenyEntry;
use kbf_state::{Cond, Store, Txn};
use kbf_types::{HostRole, NodeId, Role, RoleSet};
use prost::Message as _;

use crate::ATTEMPTS;
use crate::enroll::advertise_of;
use crate::error::{Corrupt, RegistryError};
use crate::rows::{
    self, DENY_PREFIX, DenyRow, NODE_PREFIX, NodeRow, OwnerRow, STATE_ACTIVE, STATE_REVOKED,
};

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
    /// The token it enrolled with; `adopted` for a node adopted without one.
    pub token_id: String,
    /// That token's label when it enrolled; `adopted` for an adopted node.
    pub token_label: String,
    /// Store-clock seconds of its enrollment.
    pub created_at: u64,
    /// Store-clock seconds of its last renewal.
    pub last_renewed_at: Option<u64>,
    /// When its newest certificate expires; 0 for an adopted node, which holds none.
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
    let names: Vec<String> = if row.adopted_roles.is_empty() {
        row.certs.iter().map(|c| c.role.clone()).collect()
    } else {
        row.adopted_roles.clone()
    };
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

/// The `token_id` and `token_label` of a node adopted into the registry without a token.
pub const ADOPTED: &str = "adopted";

/// Records a node that was provisioned before enrollment, so the registry knows its id and
/// the storage address it advertises (the join of storage health and of safe-to-stop goes
/// through that address). One transaction writes the same `enroll/node/<id>` row an
/// enrollment writes, active, with `token_id` and `token_label` [`ADOPTED`] and no
/// certificate: no serial, no key and no renewal (a renewal of it is refused as
/// [`kbf_types::RefusalReason::UnknownNode`]); a revoke denies the node id alone. The
/// address is stored as [`crate::canonical_addr`] spells it. Returns the row as
/// [`list_nodes`] shows it.
///
/// # Errors
/// [`RegistryError::Invalid`] for the role `updater`, a storage node without a
/// well-formed `host:port` or another node with one; [`RegistryError::AlreadyExists`] for an
/// id the registry holds (revoked ones included) or an address another node holds; the store
/// failing; [`RegistryError::Contended`].
pub fn adopt_node(
    store: &dyn Store,
    id: &NodeId,
    roles: RoleSet,
    advertise: &str,
) -> Result<NodeInfo, RegistryError> {
    if roles.contains(Role::Updater) {
        return Err(RegistryError::Invalid(
            "a host's updater enrolls; it cannot be adopted",
        ));
    }
    let advertise = advertise_of(roles, advertise).map_err(|_| {
        RegistryError::Invalid(
            "a storage node needs a host:port advertise address; any other must give none",
        )
    })?;
    let node_key = rows::node_key(id);
    for _ in 0..ATTEMPTS {
        if store.get(&node_key)?.is_some() {
            return Err(RegistryError::AlreadyExists(
                "the node id is in the registry",
            ));
        }
        let mut txn = Txn::new();
        if let Some(host) = &advertise {
            let key = rows::advertise_key(host);
            if store.get(&key)?.is_some() {
                return Err(RegistryError::AlreadyExists(
                    "another node holds the advertise address",
                ));
            }
            let owner = OwnerRow {
                node_id: id.to_string(),
                role: Role::Storage.name().to_owned(),
            };
            txn = txn.put(key, owner.encode_to_vec(), Cond::Absent);
        }
        let row = NodeRow {
            advertise: advertise.clone().unwrap_or_default(),
            token_id: ADOPTED.to_owned(),
            token_label: ADOPTED.to_owned(),
            state: STATE_ACTIVE,
            created_at: rows::seconds(store.now()),
            adopted_roles: roles.iter().map(|r| r.name().to_owned()).collect(),
            ..NodeRow::default()
        };
        let txn = txn.put(&node_key, row.encode_to_vec(), Cond::Absent);
        if store.txn(txn)?.is_ok() {
            return Ok(node_info(id.clone(), &row)?);
        }
    }
    Err(RegistryError::Contended)
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

/// What the host `id` runs, as its updater enrolled: `None` for an id that has no updater
/// row. A renewal asks it, to issue the certificate the host already holds.
///
/// # Errors
/// The store failing; a row that does not decode.
pub fn updater_host_role(
    store: &dyn Store,
    id: &NodeId,
) -> Result<Option<HostRole>, RegistryError> {
    let read = rows::fetch::<NodeRow>(store, &rows::updater_key(id), "node")?;
    Ok(read
        .map(|read| crate::tokens::host_role_of(read.row.host_role.as_deref(), "node"))
        .transpose()?
        .flatten())
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

/// Revokes a host in one transaction: its node row becomes `Revoked` (its id is burned) and
/// deny rows are written for the node id, every unexpired serial it holds or held and the
/// hash of each of its keys. The host's updater row, when it has one, is revoked in the same
/// transaction with its serials and keys denied, so its renewal is refused; an updater with
/// no node row is revoked alone and burns no id. Returns the entries written.
///
/// # Errors
/// [`RegistryError::NotFound`] for a host with neither row; [`RegistryError::Invalid`] for a
/// host with nothing left to revoke; the store failing; [`RegistryError::Contended`].
pub fn revoke_node(
    store: &dyn Store,
    id: &NodeId,
    by: &str,
) -> Result<Vec<DenyEntry>, RegistryError> {
    let node_key = rows::node_key(id);
    let updater_key = rows::updater_key(id);
    for _ in 0..ATTEMPTS {
        let node = rows::fetch::<NodeRow>(store, &node_key, "node")?;
        let updater = rows::fetch::<NodeRow>(store, &updater_key, "node")?;
        if node.is_none() && updater.is_none() {
            return Err(RegistryError::NotFound);
        }
        let now = rows::seconds(store.now());
        let mut txn = Txn::new();
        let mut keys = Vec::new();
        for (key, read, with_node) in [(&node_key, node, true), (&updater_key, updater, false)] {
            let Some(read) = read else { continue };
            if state_of(&read.row)? == NodeState::Revoked {
                continue;
            }
            let mut row = read.row;
            keys.extend(deny_keys(id, &row, now, with_node));
            row.state = STATE_REVOKED;
            row.revoked_at = Some(now);
            row.revoked_by = by.to_owned();
            txn = txn.put(key, row.encode_to_vec(), Cond::Version(read.version));
            // The id stays burned, the address does not: another node may take it.
            if !row.advertise.is_empty() {
                txn = txn.delete(rows::advertise_key(&row.advertise), Cond::Any);
            }
        }
        if keys.is_empty() {
            return Err(RegistryError::Invalid("the node is already revoked"));
        }
        keys.sort();
        keys.dedup();
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
