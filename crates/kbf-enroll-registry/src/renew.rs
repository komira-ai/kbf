//! The renew transaction (design section 5.8).

use std::collections::BTreeMap;
use std::time::Duration;

use kbf_state::{Cond, Store, Txn};
use kbf_types::{NodeId, RefusalReason, Role, RoleSet};
use prost::Message as _;

use crate::ATTEMPTS;
use crate::enroll::{Enrolled, IssuedRole, cert_row, check_issued, request_hash, stored_certs};
use crate::error::Fail;
use crate::nodes::{NodeState, state_of};
use crate::rows::{self, CertRow, NodeRow, PriorRow, SerialRow};

/// How far a request's `issued_at` may be from the store's clock.
pub const ISSUED_AT_WINDOW: u64 = 5 * 60;
/// How long a `(node, CSR)` pair is remembered as seen.
pub const REPLAY_TTL: Duration = Duration::from_secs(10 * 60);
/// The least time between two renewals of a node.
pub const MIN_INTERVAL: u64 = 60;
/// How long after its last certificate expired a node can still renew.
pub const MAX_EXPIRED_AGE: u64 = 30 * 24 * 3600;

/// One role of a `Renew` request. The caller has validated the CSR and hashed its key.
#[derive(Clone, Copy, Debug)]
pub struct RenewCsr<'a> {
    /// The role.
    pub role: Role,
    /// The CSR, DER.
    pub csr_der: &'a [u8],
    /// SHA-256 of the SubjectPublicKeyInfo the CSR carries.
    pub spki_sha256: [u8; 32],
}

/// A `Renew` request, as far as the registry needs it.
#[derive(Clone, Copy, Debug)]
pub struct RenewRequest<'a> {
    /// The node.
    pub node_id: &'a NodeId,
    /// One per role the node holds.
    pub csrs: &'a [RenewCsr<'a>],
    /// Unix seconds, as the CSRs signed it.
    pub issued_at: u64,
}

/// Renews a node's certificates. The node must exist and not be revoked, ask for exactly
/// the roles it holds with the key the registry holds for each, and be within the time
/// rules; the new serials are recorded with the old ones' `superseded_at`, in one
/// transaction that carries the node row's version, so a revocation that lands meanwhile
/// makes it fail and the decision is made again (and refused). The registry keeps the key
/// it holds whatever `sign` returns. `sign` is called, with no store access, only after
/// every check.
///
/// # Errors
/// The [`RefusalReason`] the frontend sends, mapped as for [`crate::enroll_txn`].
pub fn renew_txn(
    store: &dyn Store,
    req: &RenewRequest<'_>,
    sign: &mut dyn FnMut() -> Result<Vec<IssuedRole>, RefusalReason>,
) -> Result<Enrolled, RefusalReason> {
    unimplemented!("stub: the red-first commit has signatures only")
}

fn attempt(
    store: &dyn Store,
    req: &RenewRequest<'_>,
    sign: &mut dyn FnMut() -> Result<Vec<IssuedRole>, RefusalReason>,
) -> Result<Enrolled, Fail> {
    let asked =
        RoleSet::new(req.csrs.iter().map(|c| c.role)).map_err(|_| RefusalReason::BadRoles)?;
    let pairs: Vec<(Role, &[u8])> = req.csrs.iter().map(|c| (c.role, c.csr_der)).collect();
    let csr_hash = request_hash(&pairs);
    for _ in 0..ATTEMPTS {
        let now = rows::seconds(store.now());
        let key = rows::node_key(req.node_id);
        let node =
            rows::fetch::<NodeRow>(store, &key, "node")?.ok_or(RefusalReason::UnknownNode)?;
        if state_of(&node.row)? == NodeState::Revoked {
            return Err(RefusalReason::NodeRevoked.into());
        }
        let held = stored_certs(&node.row)?;
        let newest = held.iter().map(|c| c.not_after).max().unwrap_or(0);
        if now > newest + MAX_EXPIRED_AGE {
            return Err(RefusalReason::RenewalExpired.into());
        }
        let roles = RoleSet::new(held.iter().map(|c| c.role))
            .map_err(|_| RefusalReason::StoreUnavailable)?;
        if asked != roles {
            return Err(RefusalReason::BadRoles.into());
        }
        let wrong_key = req.csrs.iter().any(|c| {
            held.iter()
                .any(|h| h.role == c.role && h.spki_sha256 != c.spki_sha256)
        });
        if wrong_key {
            return Err(RefusalReason::WrongKey.into());
        }
        if req.issued_at.abs_diff(now) > ISSUED_AT_WINDOW {
            return Err(RefusalReason::StaleIssuedAt.into());
        }
        let seen = rows::renew_key(req.node_id, &csr_hash);
        let recent = node
            .row
            .last_renewed_at
            .is_some_and(|at| now < at.saturating_add(MIN_INTERVAL));
        if store.get(&seen)?.is_some() || recent {
            return Err(RefusalReason::RenewTooSoon.into());
        }

        let issued = sign()?;
        check_issued(roles, &issued)?;

        let keys: BTreeMap<Role, [u8; 32]> = held.iter().map(|h| (h.role, h.spki_sha256)).collect();
        let mut row = node.row.clone();
        row.certs = issued
            .iter()
            .map(|i| CertRow {
                // Key continuity: the registry's key, whatever the signer says. (The
                // roles were checked equal, so every role has one.)
                spki_sha256: keys.get(&i.role).map(|k| k.to_vec()).unwrap_or_default(),
                ..cert_row(i)
            })
            .collect();
        let mut txn = Txn::new();
        for old in &held {
            txn = txn.put(
                rows::serial_key(&old.serial),
                SerialRow {
                    node_id: req.node_id.to_string(),
                    role: old.role.name().to_owned(),
                    not_after: old.not_after,
                    superseded_at: Some(now),
                }
                .encode_to_vec(),
                Cond::Any,
            );
        }
        row.priors = held
            .iter()
            .map(|h| (h.role.name(), h.serial.as_str(), h.not_after))
            .chain(
                node.row
                    .priors
                    .iter()
                    .map(|p| (p.role.as_str(), p.serial.as_str(), p.not_after)),
            )
            .filter(|(_, _, not_after)| *not_after > now)
            .map(|(role, serial, not_after)| PriorRow {
                role: role.to_owned(),
                serial: serial.to_owned(),
                not_after,
            })
            .collect();
        row.last_renewed_at = Some(now);
        txn = txn
            .put(&key, row.encode_to_vec(), Cond::Version(node.version))
            .put_ttl(&seen, Vec::new(), REPLAY_TTL, Cond::Absent);
        for cert in &row.certs {
            txn = txn.put(
                rows::serial_key(&cert.serial),
                SerialRow {
                    node_id: req.node_id.to_string(),
                    role: cert.role.clone(),
                    not_after: cert.not_after,
                    superseded_at: None,
                }
                .encode_to_vec(),
                Cond::Absent,
            );
        }
        if store.txn(txn)?.is_ok() {
            return Ok(Enrolled {
                replayed: false,
                certs: stored_certs(&row)?,
            });
        }
    }
    Err(RefusalReason::StoreUnavailable.into())
}
