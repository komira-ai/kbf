//! `Renew` (`docs/design/install-and-enrollment.md`, section 5.8).
//!
//! The caller proves itself by the key the registry holds, not by a token or a client
//! certificate: each role's CSR is signed by that key and carries `issued_at` among its
//! signed attributes. The checks, in order:
//!
//! 1. the shape (the node id, the role names, a CSR no longer than the CA reads);
//! 2. each CSR parses ([`kbf_ca::parse_csr`]: P-256, `ecdsa-with-SHA256`, a signature that
//!    verifies) and its `issued_at` attribute **equals** `RenewRequest.issued_at`: the
//!    field is not signed, so a replay of an old CSR under a new `issued_at` is refused
//!    here as [`RefusalReason::MalformedRequest`] rather than trusted;
//! 3. `kbf_enroll_registry::renew_txn`: the node exists and is not revoked
//!    ([`RefusalReason::UnknownNode`], [`RefusalReason::NodeRevoked`]), its last
//!    certificate did not expire too long ago, it asks for exactly the roles it holds
//!    with the key the registry holds for each ([`RefusalReason::WrongKey`]), `issued_at`
//!    is within five minutes of the store's clock ([`RefusalReason::StaleIssuedAt`]), and
//!    the same CSR was not seen lately and the node did not renew within the last minute
//!    ([`RefusalReason::RenewTooSoon`]);
//! 4. the certificates are issued for the same keys and recorded in one transaction.
//!
//! A storage node's certificate names its advertise host as at enrollment; the host is
//! read from the registry row before the transaction. (A re-enrollment that changed it
//! changed the keys too, so a renewal that passes the key check cannot meet a different
//! host.)

use kbf_ca::parse_csr;
use kbf_enroll_registry::{self as registry, RenewCsr, list_nodes};
use kbf_proto::kbf::enroll::v1 as pb;
use kbf_types::{NodeId, RefusalReason, Role};
use sha2::{Digest as _, Sha256};

use super::service::{
    CONFIG_GENERATION, EnrollService, certs_of, csrs_of, registry_reason, renew_after,
};

impl EnrollService {
    /// A `Renew` call, run here (it blocks on the store).
    ///
    /// # Errors
    /// The [`RefusalReason`] the frontend sends.
    pub fn renew_now(&self, request: &pb::RenewRequest) -> Result<pb::RenewResponse, RefusalReason> {
        let core = &*self.core;
        let node_id: NodeId = request
            .node_id
            .parse()
            .map_err(|_| RefusalReason::MalformedRequest)?;
        let csrs = csrs_of(&request.csrs)?;
        let issued_at =
            u64::try_from(request.issued_at).map_err(|_| RefusalReason::StaleIssuedAt)?;
        let mut keys = Vec::with_capacity(csrs.len());
        let mut asked = Vec::with_capacity(csrs.len());
        for (role, der) in &csrs {
            let parsed = parse_csr(der).map_err(|_| RefusalReason::BadCsr)?;
            if parsed.issued_at != Some(request.issued_at) {
                return Err(RefusalReason::MalformedRequest);
            }
            asked.push(RenewCsr {
                role: *role,
                csr_der: der,
                spki_sha256: Sha256::digest(parsed.key.spki()).into(),
            });
            keys.push((*role, parsed.key));
        }
        let advertise = advertise_of(self, &node_id)?;
        let mut sign = || {
            keys.iter()
                .map(|(role, key)| {
                    let named = advertise.as_deref().filter(|_| *role == Role::Storage);
                    core.issue(&node_id, *role, key, named)
                })
                .collect()
        };
        let renewed = registry::renew_txn(
            &*core.store,
            &registry::RenewRequest {
                node_id: &node_id,
                csrs: &asked,
                issued_at,
            },
            &mut sign,
        )?;
        let now = core.now();
        Ok(pb::RenewResponse {
            certs: certs_of(&renewed.certs),
            ca_bundle_pem: core.ca.bundle_pem().into_bytes(),
            config: (request.config_generation != CONFIG_GENERATION)
                .then(|| core.config(&renewed)),
            server_time: now,
            renew_after: renew_after(now, &renewed.certs),
        })
    }
}

/// The advertise host of `node`'s registry row (without the port), if it has one. A node
/// that is not in the registry has none here and is told `UnknownNode` by the transaction.
fn advertise_of(service: &EnrollService, node: &NodeId) -> Result<Option<String>, RefusalReason> {
    let nodes = list_nodes(&*service.core.store).map_err(|e| registry_reason(&e))?;
    Ok(nodes
        .into_iter()
        .find(|info| &info.id == node)
        .and_then(|info| {
            info.advertise
                .rsplit_once(':')
                .map(|(host, _)| host.to_owned())
        }))
}

#[cfg(test)]
mod tests;
