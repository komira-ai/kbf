//! The enroll transaction (design sections 5.6 and 5.10).
//!
//! Order of decisions, once the request has the right shape: the secret is verified; a
//! request this token already served is answered from the stored node (no second use, no
//! recheck of the token); then the token's policy (revoked, expired, used up, roles, node id,
//! names), the node id and advertise host, and the keys; the signer is called; one
//! transaction then spends the use and writes the node, its serials, its keys, its
//! idempotency row and, for a replacement, the deny rows of the node it replaces.
//! The transaction carries the version of every row it decided on, so a racing writer
//! makes it fail and the whole decision is made again on fresh rows.

use std::collections::BTreeSet;

use kbf_state::{Cond, Store, Txn};
use kbf_types::{NodeId, RefusalReason, Role, RoleSet, TokenId};
use prost::Message as _;
use sha2::{Digest as _, Sha256};
use subtle::{Choice, ConstantTimeEq as _};

use crate::ATTEMPTS;
use crate::error::Fail;
use crate::nodes::{NodeState, deny_keys, state_of, with_deny_rows};
use crate::rows::{self, CertRow, NodeRow, OwnerRow, Read, STATE_ACTIVE, SerialRow, TokenRow};
use crate::tokens::secret_digest;

/// What a token's secret is compared with when its id is unknown, so a wrong id and a wrong
/// secret do the same work. No secret hashes to it.
const DUMMY_DIGEST: [u8; 32] = [0x5a; 32];

/// A certificate the signer issued for one role.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssuedRole {
    /// The role it is for.
    pub role: Role,
    /// SHA-256 of the certificate's SubjectPublicKeyInfo.
    pub spki_sha256: [u8; 32],
    /// The serial, big-endian; the registry spells it as the deny list does.
    pub serial: Vec<u8>,
    /// Unix seconds.
    pub not_before: u64,
    /// Unix seconds.
    pub not_after: u64,
    /// The certificate, DER.
    pub cert_der: Vec<u8>,
}

/// A certificate as the registry holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredCert {
    /// The role it is for.
    pub role: Role,
    /// SHA-256 of the SubjectPublicKeyInfo.
    pub spki_sha256: [u8; 32],
    /// Lower-case hex without leading zeros.
    pub serial: String,
    /// Unix seconds.
    pub not_before: u64,
    /// Unix seconds.
    pub not_after: u64,
    /// The certificate, DER.
    pub cert_der: Vec<u8>,
}

/// The result of an enroll or renew that went through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Enrolled {
    /// True when the answer is the stored one for a request already served; no use was
    /// spent and the signer was not called.
    pub replayed: bool,
    /// One certificate per role, in [`Role::ALL`] order.
    pub certs: Vec<StoredCert>,
}

/// An `Enroll` request, as far as the registry needs it. The caller has parsed the ids and
/// validated each CSR (the registry reads none).
#[derive(Clone, Copy, Debug)]
pub struct EnrollRequest<'a> {
    /// The token's lookup id.
    pub token_id: &'a TokenId,
    /// The secret bytes as presented; anything but 32 bytes is refused as a wrong secret.
    pub secret: &'a [u8],
    /// The node id asked for.
    pub node_id: &'a NodeId,
    /// One CSR (DER) per role.
    pub csrs: &'a [(Role, &'a [u8])],
    /// `host:port`, for a storage node; empty otherwise.
    pub advertise: &'a str,
    /// The zone label.
    pub zone: &'a str,
}

/// The idempotency hash of a request's CSRs: SHA-256 over, in [`Role::ALL`] order, the role
/// name, a zero byte, the CSR's length as four big-endian bytes and the CSR.
#[must_use]
pub fn request_hash(csrs: &[(Role, &[u8])]) -> [u8; 32] {
    unimplemented!("stub: the red-first commit has signatures only")
}

pub(crate) fn stored_certs(row: &NodeRow) -> Result<Vec<StoredCert>, crate::error::Corrupt> {
    let mut certs = row
        .certs
        .iter()
        .map(|c| {
            Ok(StoredCert {
                role: rows::role_of(&c.role, "node")?,
                spki_sha256: c
                    .spki_sha256
                    .as_slice()
                    .try_into()
                    .map_err(|_| crate::error::Corrupt("node key"))?,
                serial: c.serial.clone(),
                not_before: c.not_before,
                not_after: c.not_after,
                cert_der: c.cert_der.clone(),
            })
        })
        .collect::<Result<Vec<_>, crate::error::Corrupt>>()?;
    certs.sort_by_key(|c| c.role);
    Ok(certs)
}

/// Checks what the signer returned against what was asked: the roles are exactly the
/// requested ones, and no serial or key repeats.
pub(crate) fn check_issued(roles: RoleSet, issued: &[IssuedRole]) -> Result<(), Fail> {
    let got = RoleSet::new(issued.iter().map(|i| i.role)).map_err(|_| RefusalReason::BadRoles)?;
    if got != roles {
        return Err(RefusalReason::BadRoles.into());
    }
    let serials: BTreeSet<String> = issued
        .iter()
        .map(|i| rows::serial_text(&i.serial))
        .collect();
    let keys: BTreeSet<[u8; 32]> = issued.iter().map(|i| i.spki_sha256).collect();
    if serials.len() != issued.len() || keys.len() != issued.len() {
        return Err(RefusalReason::BadCsr.into());
    }
    Ok(())
}

pub(crate) fn cert_row(i: &IssuedRole) -> CertRow {
    CertRow {
        role: i.role.name().to_owned(),
        spki_sha256: i.spki_sha256.to_vec(),
        serial: rows::serial_text(&i.serial),
        not_before: i.not_before,
        not_after: i.not_after,
        cert_der: i.cert_der.clone(),
    }
}

/// The `host:port` a storage node advertises, lower-cased, or `None` for a node with no
/// storage role. A storage node must name one; any other must not.
fn advertise_of(roles: RoleSet, text: &str) -> Result<Option<String>, Fail> {
    if !roles.contains(Role::Storage) {
        return if text.is_empty() {
            Ok(None)
        } else {
            Err(RefusalReason::MalformedRequest.into())
        };
    }
    let text = text.to_ascii_lowercase();
    let well_formed = text
        .rsplit_once(':')
        .is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok_and(|p| p > 0));
    if well_formed {
        Ok(Some(text))
    } else {
        Err(RefusalReason::MalformedRequest.into())
    }
}

/// Whether a token's `names` allow `host_port`: no names allows any; a name equals the host
/// or, when it starts with `.`, is a suffix of it.
fn names_allow(names: &[String], host_port: &str) -> bool {
    let host = host_port
        .rsplit_once(':')
        .map_or(host_port, |(host, _)| host);
    names.is_empty()
        || names.iter().any(|name| {
            let name = name.to_ascii_lowercase();
            host == name || (name.starts_with('.') && host.ends_with(&name))
        })
}

/// The token row, if `secret` is right for it. An unknown id and a wrong secret are the same
/// answer after the same work.
fn verify_secret(stored: Option<Read<TokenRow>>, secret: &[u8]) -> Result<Read<TokenRow>, Fail> {
    let presented = secret_digest(secret);
    let expected = stored
        .as_ref()
        .and_then(|read| <[u8; 32]>::try_from(read.row.secret_sha256.as_slice()).ok())
        .unwrap_or(DUMMY_DIGEST);
    let known = Choice::from(u8::from(stored.is_some()));
    let long_enough = Choice::from(u8::from(secret.len() == 32));
    let right = bool::from(known & long_enough & presented.ct_eq(&expected));
    match stored {
        Some(token) if right => Ok(token),
        _ => Err(RefusalReason::TokenRefused.into()),
    }
}

fn check_policy(
    token: &TokenRow,
    now: u64,
    roles: RoleSet,
    node: &NodeId,
    host_port: Option<&str>,
) -> Result<(), RefusalReason> {
    if token.revoked_at.is_some() {
        return Err(RefusalReason::TokenRevoked);
    }
    if now >= token.expires_at {
        return Err(RefusalReason::TokenExpired);
    }
    if token.uses >= token.max_uses {
        return Err(RefusalReason::TokenUsedUp);
    }
    let granted = RoleSet::from_names(token.roles.iter().map(String::as_str))
        .map_err(|_| RefusalReason::StoreUnavailable)?;
    if roles.iter().any(|role| !granted.contains(role)) {
        return Err(RefusalReason::RoleNotGranted);
    }
    let exact = token.node_id.as_ref().is_some_and(|id| id != node.as_str());
    let prefixed = token
        .node_prefix
        .as_ref()
        .is_some_and(|prefix| !node.as_str().starts_with(prefix.as_str()));
    if exact || prefixed {
        return Err(RefusalReason::NodeIdNotAllowed);
    }
    if host_port.is_some_and(|host| !names_allow(&token.names, host)) {
        return Err(RefusalReason::AdvertiseNotAllowed);
    }
    Ok(())
}

/// The stored answer for a request this token already served, if the node is still active
/// with unexpired certificates.
fn replay(
    store: &dyn Store,
    req: &EnrollRequest<'_>,
    csr_hash: &[u8; 32],
    now: u64,
) -> Result<Option<Enrolled>, Fail> {
    let key = rows::request_key(req.token_id, csr_hash);
    let Some(owner) = rows::fetch::<OwnerRow>(store, &key, "request")? else {
        return Ok(None);
    };
    if owner.row.node_id != req.node_id.as_str() {
        return Err(RefusalReason::MalformedRequest.into());
    }
    let Some(node) = rows::fetch::<NodeRow>(store, &rows::node_key(req.node_id), "node")? else {
        return Ok(None);
    };
    let certs = stored_certs(&node.row)?;
    let live = state_of(&node.row)? == NodeState::Active && certs.iter().all(|c| c.not_after > now);
    Ok(live.then_some(Enrolled {
        replayed: true,
        certs,
    }))
}

/// Enrolls a node: see the module documentation for the order of decisions. `sign` is
/// called, with no store access, once the request has passed every check and before the
/// transaction; if the transaction then loses a race the whole decision is made again and
/// `sign` is called again (a wasted signature, never a recorded serial).
///
/// # Errors
/// The [`RefusalReason`] the frontend sends. A fenced handle is
/// [`RefusalReason::NotSigner`]; any other store failure, a row that does not decode and
/// a race lost [`ATTEMPTS`] times are [`RefusalReason::StoreUnavailable`].
pub fn enroll_txn(
    store: &dyn Store,
    req: &EnrollRequest<'_>,
    sign: &mut dyn FnMut() -> Result<Vec<IssuedRole>, RefusalReason>,
) -> Result<Enrolled, RefusalReason> {
    unimplemented!("stub: the red-first commit has signatures only")
}

fn attempt(
    store: &dyn Store,
    req: &EnrollRequest<'_>,
    sign: &mut dyn FnMut() -> Result<Vec<IssuedRole>, RefusalReason>,
) -> Result<Enrolled, Fail> {
    let roles = RoleSet::new(req.csrs.iter().map(|(role, _)| *role))
        .map_err(|_| RefusalReason::BadRoles)?;
    let host_port = advertise_of(roles, req.advertise)?;
    let csr_hash = request_hash(req.csrs);
    for _ in 0..ATTEMPTS {
        if let Some(done) =
            decide_and_commit(store, req, roles, host_port.as_deref(), &csr_hash, sign)?
        {
            return Ok(done);
        }
    }
    Err(RefusalReason::StoreUnavailable.into())
}

/// One pass over fresh rows: `Some` when it ended (answered, or committed), `None` when the
/// transaction lost a race.
fn decide_and_commit(
    store: &dyn Store,
    req: &EnrollRequest<'_>,
    roles: RoleSet,
    host_port: Option<&str>,
    csr_hash: &[u8; 32],
    sign: &mut dyn FnMut() -> Result<Vec<IssuedRole>, RefusalReason>,
) -> Result<Option<Enrolled>, Fail> {
    let now = rows::seconds(store.now());
    let token = rows::fetch::<TokenRow>(store, &rows::token_key(req.token_id), "token")?;
    let token = verify_secret(token, req.secret)?;
    if let Some(done) = replay(store, req, csr_hash, now)? {
        return Ok(Some(done));
    }
    check_policy(&token.row, now, roles, req.node_id, host_port)?;

    let node_key = rows::node_key(req.node_id);
    let existing = rows::fetch::<NodeRow>(store, &node_key, "node")?;
    let (node_cond, replaced) = match existing {
        None => (Cond::Absent, None),
        Some(old) if state_of(&old.row)? == NodeState::Revoked => {
            return Err(RefusalReason::NodeRevoked.into());
        }
        Some(_) if !token.row.replace => return Err(RefusalReason::NodeIdInUse.into()),
        Some(old) => (Cond::Version(old.version), Some(old)),
    };

    let (adv_cond, adv_key) = match host_port {
        None => (Cond::Any, None),
        Some(host) => {
            let key = rows::advertise_key(host);
            let cond = match rows::fetch::<OwnerRow>(store, &key, "advertise")? {
                None => Cond::Absent,
                Some(owner) if owner.row.node_id == req.node_id.as_str() => {
                    Cond::Version(owner.version)
                }
                Some(_) => return Err(RefusalReason::AdvertiseInUse.into()),
            };
            (cond, Some(key))
        }
    };

    let issued = sign()?;
    check_issued(roles, &issued)?;
    for cert in &issued {
        if store.get(&rows::spki_key(&cert.spki_sha256))?.is_some() {
            return Err(RefusalReason::BadCsr.into());
        }
    }

    let mut token_row = token.row.clone();
    token_row.uses += 1;
    let node_row = NodeRow {
        certs: issued.iter().map(cert_row).collect(),
        advertise: host_port.unwrap_or_default().to_owned(),
        zone: req.zone.to_owned(),
        token_id: req.token_id.to_string(),
        token_label: token.row.label.clone(),
        state: STATE_ACTIVE,
        created_at: now,
        ..NodeRow::default()
    };
    let ttl = std::time::Duration::from_secs(token.row.expires_at.saturating_sub(now) + 24 * 3600);
    let mut txn = Txn::new()
        .put(
            rows::token_key(req.token_id),
            token_row.encode_to_vec(),
            Cond::Version(token.version),
        )
        .put(&node_key, node_row.encode_to_vec(), node_cond)
        .put_ttl(
            rows::request_key(req.token_id, csr_hash),
            OwnerRow {
                node_id: req.node_id.to_string(),
                role: String::new(),
            }
            .encode_to_vec(),
            ttl,
            // A row left by a request that did not stand (its node was revoked or purged)
            // is overwritten; the node row's own condition is what keeps two racing
            // copies of one request from both landing.
            Cond::Any,
        );
    for cert in &node_row.certs {
        txn = txn
            .put(
                rows::serial_key(&cert.serial),
                SerialRow {
                    node_id: req.node_id.to_string(),
                    role: cert.role.clone(),
                    not_after: cert.not_after,
                    superseded_at: None,
                }
                .encode_to_vec(),
                Cond::Absent,
            )
            .put(
                rows::spki_key(&cert.spki_sha256),
                OwnerRow {
                    node_id: req.node_id.to_string(),
                    role: cert.role.clone(),
                }
                .encode_to_vec(),
                Cond::Absent,
            );
    }
    if let Some(key) = &adv_key {
        let owner = OwnerRow {
            node_id: req.node_id.to_string(),
            role: Role::Storage.name().to_owned(),
        };
        txn = txn.put(key, owner.encode_to_vec(), adv_cond);
    }
    if let Some(old) = replaced {
        let stale_adv = (!old.row.advertise.is_empty()
            && Some(old.row.advertise.as_str()) != host_port)
            .then(|| rows::advertise_key(&old.row.advertise));
        if let Some(key) = stale_adv {
            txn = txn.delete(key, Cond::Any);
        }
        let keys = deny_keys(req.node_id, &old.row, now, false);
        txn = with_deny_rows(txn, &keys, now, &token.row.label, "replaced");
    }
    if store.txn(txn)?.is_err() {
        return Ok(None);
    }
    Ok(Some(Enrolled {
        replayed: false,
        certs: stored_certs(&node_row)?,
    }))
}
