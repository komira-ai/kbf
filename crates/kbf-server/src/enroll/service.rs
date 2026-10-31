//! `kbf.enroll.v1.Enroll` on the frontend (`docs/design/install-and-enrollment.md`, sections
//! 5.3 to 5.8): [`EnrollService::enroll_now`] and, in [`super::renew`],
//! `renew_now`, over the registry in the state store (`kbf-enroll-registry`) and the CA
//! ([`super::ca::Ca`]).
//!
//! **Order of an `Enroll`.** The shape of the request (ids, role names, the size of each
//! CSR) is checked first and reads nothing. Then `kbf_enroll_registry::enroll_txn` runs:
//! it verifies the token secret, answers a request it already served from the stored
//! node, checks the token's policy (revoked, expired, used up, the roles, the node id,
//! the advertise names), the node id and the advertise host, and only then calls the
//! signing closure built here; the closure parses each CSR with [`kbf_ca::parse_csr`]
//! (P-256, `ecdsa-with-SHA256`, 2 KiB, a signature that verifies), refuses a node id that
//! is `localhost` or a name the frontend serves ([`RefusalReason::NodeIdNotAllowed`]) and an
//! advertise host the frontend will not certify, and builds each certificate with
//! [`kbf_ca::Authority::issue_node`]. A CSR is therefore parsed for a caller who has
//! proved the token secret, never for a guess.
//!
//! **Refusals** are exactly [`RefusalReason`]: the status code is `reason.code()` and the
//! message `reason.message()`, byte for byte, because the node client matches a refusal
//! only on both. A wrong secret and an unknown id are the same reason with the same work
//! (the registry compares against a dummy digest). A fenced state-store handle is
//! [`RefusalReason::NotSigner`] and any other store failure
//! [`RefusalReason::StoreUnavailable`]; a CA that cannot sign right now (its signer
//! process is down, the clock is before the build's floor) is `NotSigner` too, since the
//! closed set has no other "this process cannot sign" reason and a node retries it.
//!
//! **The answer** carries one certificate per role, the CA bundle, the frontend's clock,
//! when to renew (half-way through the shortest certificate's life) and the
//! `NodeConfig` the node pulls. Each call blocks on the store, so it runs on the blocking
//! pool.

use std::net::IpAddr;
use std::sync::Arc;

use kbf_ca::{CaError, Issued, NodeCert, PublicKey, SystemEntropy, UpdaterCert, parse_csr};
use kbf_enroll_registry::{self as registry, Enrolled, IssuedRole, StoredCert};
use kbf_proto::kbf::enroll::v1 as pb;
use kbf_proto::kbf::enroll::v1::enroll_server::Enroll;
use kbf_state::Store;
use kbf_types::{HostRole, NodeId, RefusalReason, Role, RoleSet, TokenId};
use sha2::{Digest as _, Sha256};
use tonic::{Request, Response, Status};

use super::ca::Ca;
use crate::object_store::{ColdStoreView, RegistryError, cold_store_view};

/// The longest zone label accepted.
const MAX_ZONE: usize = 63;
/// The longest `host:port` accepted (a DNS name, a colon and a port).
const MAX_ADVERTISE: usize = 260;
/// Seconds between a node's looks at its certificate files, as the answer tells it.
const TLS_RELOAD_INTERVAL_S: u32 = 30;

/// What the frontend tells every node about the farm (the `--enroll-*-endpoint` and
/// `--enroll-storage-port` flags). Nothing here is a secret.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeSettings {
    /// The https URL a daemon dials; empty for none.
    pub worker_endpoint: String,
    /// What `--cas` is for a daemon-only node; empty for none.
    pub cas_endpoint: String,
    /// The port a storage node serves on; 0 for none.
    pub storage_port: u32,
}

/// The generation of `config`: a digest of everything else in it, so it differs exactly
/// when something the node receives differs (a moved worker, CAS or storage endpoint, a
/// registered, changed or withdrawn cold store, other roles, the reload interval) and a
/// restarted frontend with other `--enroll-*` flags needs no counter anyone must raise.
/// It is not ordered: a node compares it for equality only (`Renew` sends the
/// configuration when it is not the node's), so a frontend put back to an earlier endpoint
/// sends that configuration again. The low bit is set so it is never 0, the number of a
/// node that holds nothing.
fn content_generation(config: &pb::NodeConfig) -> u64 {
    let digest = Sha256::digest(prost::Message::encode_to_vec(&pb::NodeConfig {
        generation: 0,
        ..config.clone()
    }));
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(head) | 1
}

/// The status of a refusal: its code and its message, nothing else.
#[must_use]
pub fn status_of(reason: RefusalReason) -> Status {
    Status::new(
        tonic::Code::from_i32(reason.code().number()),
        reason.message(),
    )
}

/// What the registry says when an operator-style call (a read) fails here.
pub(super) fn registry_reason(error: &registry::RegistryError) -> RefusalReason {
    match error {
        registry::RegistryError::Store(kbf_state::StoreError::Fenced) => RefusalReason::NotSigner,
        _ => RefusalReason::StoreUnavailable,
    }
}

/// The reason a CA failure is told as.
fn ca_reason(error: &CaError) -> RefusalReason {
    tracing::warn!(%error, "the CA could not issue a certificate");
    match error {
        CaError::Name | CaError::AdvertiseForDaemon => RefusalReason::AdvertiseInvalid,
        _ => RefusalReason::NotSigner,
    }
}

/// The big-endian bytes of a serial spelt as lower-case hex of any length.
fn serial_bytes(hex: &str) -> Result<Vec<u8>, RefusalReason> {
    let padded = if hex.len() % 2 == 1 {
        format!("0{hex}")
    } else {
        hex.to_owned()
    };
    padded
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|text| u8::from_str_radix(text, 16).ok())
        })
        .collect::<Option<Vec<u8>>>()
        .ok_or(RefusalReason::NotSigner)
}

/// The roles a request's CSRs name, each with its CSR, after the checks that need no state:
/// at least one, every role a name a token can grant, no role twice, no CSR over the
/// size the CA reads.
pub(super) fn csrs_of(csrs: &[pb::RoleCsr]) -> Result<Vec<(Role, &[u8])>, RefusalReason> {
    let mut out = Vec::with_capacity(csrs.len());
    for csr in csrs {
        let role: Role = csr.role.parse().map_err(|_| RefusalReason::BadRoles)?;
        if csr.csr_der.len() > kbf_ca::MAX_CSR_LEN {
            return Err(RefusalReason::BadCsr);
        }
        out.push((role, csr.csr_der.as_slice()));
    }
    RoleSet::new(out.iter().map(|(role, _)| *role)).map_err(|_| RefusalReason::BadRoles)?;
    Ok(out)
}

/// Whether `host` is a name the frontend will not certify for a storage node: a name it
/// serves (or one under it), `localhost`, or an address that is loopback, unspecified,
/// link-local or multicast, or one it serves.
///
/// An address is judged as an address: any spelling of it (zero-compressed or padded IPv6,
/// the IPv4-mapped form of an IPv4 address) is the same host, so it is compared
/// and classified in its canonical form. Other names are compared as lower-case text.
pub(super) fn advertise_refused(host: &str, frontend_names: &[String]) -> bool {
    let address = |text: &str| text.parse::<IpAddr>().ok().map(|ip| ip.to_canonical());
    if let Some(ip) = address(host) {
        let link_local = match ip {
            IpAddr::V4(v4) => v4.is_link_local(),
            IpAddr::V6(v6) => v6.segments()[0] & 0xffc0 == 0xfe80,
        };
        return ip.is_loopback()
            || ip.is_unspecified()
            || ip.is_multicast()
            || link_local
            || frontend_names.iter().any(|name| address(name) == Some(ip));
    }
    host == "localhost"
        || host.ends_with(".localhost")
        || frontend_names
            .iter()
            .any(|name| host == name || host.ends_with(&format!(".{name}")))
}

/// Whether `node` is a name no node may hold: `localhost`, a name under it, or a name the
/// frontend serves. A node id is a DNS label, and every certificate carries it, so a node
/// holding one of these could be taken for the host itself.
///
/// The comparison ignores case, though a [`NodeId`] is already lower-case, so the rule
/// holds if that ever changes.
pub(super) fn node_id_refused(node: &str, frontend_names: &[String]) -> bool {
    let id = node.to_ascii_lowercase();
    id == "localhost"
        || id.ends_with(".localhost")
        || frontend_names
            .iter()
            .any(|name| name.eq_ignore_ascii_case(&id))
}

/// What both calls share.
pub(super) struct Core {
    pub(super) store: Arc<dyn Store>,
    pub(super) ca: Arc<Ca>,
    /// The names the frontend serves, lower-case: no node may claim them.
    pub(super) frontend_names: Vec<String>,
    pub(super) settings: NodeSettings,
}

/// The `kbf.enroll.v1.Enroll` service.
#[derive(Clone)]
pub struct EnrollService {
    pub(super) core: Arc<Core>,
}

impl std::fmt::Debug for EnrollService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnrollService").finish_non_exhaustive()
    }
}

impl EnrollService {
    /// The service over `store`, which must be the frontend's fenced handle (so a
    /// frontend that has lost the writer role refuses every call with `NotSigner`), and
    /// `ca`. `frontend_names` are the names this frontend serves.
    #[must_use]
    pub fn new(
        store: Arc<dyn Store>,
        ca: Arc<Ca>,
        frontend_names: &[String],
        settings: NodeSettings,
    ) -> Self {
        Self {
            core: Arc::new(Core {
                store,
                ca,
                frontend_names: frontend_names
                    .iter()
                    .map(|name| name.to_ascii_lowercase())
                    .collect(),
                settings,
            }),
        }
    }

    /// An `Enroll` call, run here (it blocks on the store).
    ///
    /// # Errors
    /// The [`RefusalReason`] the frontend sends.
    pub fn enroll_now(
        &self,
        request: &pb::EnrollRequest,
    ) -> Result<pb::EnrollResponse, RefusalReason> {
        let core = &*self.core;
        let token_id =
            TokenId::parse(&request.token_id).map_err(|_| RefusalReason::MalformedRequest)?;
        let node_id: NodeId = request
            .node_id
            .parse()
            .map_err(|_| RefusalReason::MalformedRequest)?;
        if request.zone.len() > MAX_ZONE || request.advertise.len() > MAX_ADVERTISE {
            return Err(RefusalReason::MalformedRequest);
        }
        let csrs = csrs_of(&request.csrs)?;
        let cold = core.cold()?;
        // What an updater's certificate says the host runs is the token's, fixed when it was
        // made. The registry verifies the token before it calls the signer, so reading it
        // here tells a caller who has not proved the secret nothing.
        let host_role = if csrs.iter().any(|(role, _)| *role == Role::Updater) {
            registry::token_host_role(&*core.store, &token_id).map_err(|e| registry_reason(&e))?
        } else {
            None
        };
        let mut sign = || core.sign_enroll(&node_id, &csrs, &request.advertise, host_role.as_ref());
        let enrolled = registry::enroll_txn(
            &*core.store,
            &registry::EnrollRequest {
                token_id: &token_id,
                secret: &request.token_secret,
                node_id: &node_id,
                csrs: &csrs,
                advertise: &request.advertise,
                zone: &request.zone,
            },
            &mut sign,
        )?;
        let now = core.now();
        Ok(pb::EnrollResponse {
            certs: certs_of(&enrolled.certs),
            ca_bundle_pem: core.ca.bundle_pem().into_bytes(),
            config: Some(core.config(&enrolled, &cold)),
            server_time: now,
            renew_after: renew_after(now, &enrolled.certs),
        })
    }
}

/// The wire form of the certificates a node holds.
pub(super) fn certs_of(certs: &[StoredCert]) -> Vec<pb::IssuedCert> {
    certs
        .iter()
        .map(|cert| pb::IssuedCert {
            role: cert.role.name().to_owned(),
            leaf_der: cert.cert_der.clone(),
            serial: cert.serial.clone(),
            not_before: i64::try_from(cert.not_before).unwrap_or(i64::MAX),
            not_after: i64::try_from(cert.not_after).unwrap_or(i64::MAX),
        })
        .collect()
}

/// When the node should first try to renew: half-way from `now` to the earliest expiry.
pub(super) fn renew_after(now: i64, certs: &[StoredCert]) -> i64 {
    let earliest = certs
        .iter()
        .map(|cert| i64::try_from(cert.not_after).unwrap_or(i64::MAX))
        .min()
        .unwrap_or(now);
    now + (earliest - now).max(0) / 2
}

impl Core {
    /// The store's clock in unix seconds: the one the registry judges expiry by, and the
    /// one certificates are issued at.
    pub(super) fn now(&self) -> i64 {
        i64::try_from(self.store.now().as_duration().as_secs()).unwrap_or(i64::MAX)
    }

    /// The registered cold store, as a storage node is told it. Read before a call changes
    /// anything, so a registry that cannot be read refuses the call and spends no token.
    ///
    /// # Errors
    /// [`RefusalReason::NotSigner`] for a fenced frontend, else
    /// [`RefusalReason::StoreUnavailable`].
    pub(super) fn cold(&self) -> Result<ColdStoreView, RefusalReason> {
        cold_store_view(&*self.store).map_err(|error| {
            // The text of a registry error names a row family and never a value.
            tracing::warn!(%error, "the registered cold store could not be read");
            match error {
                RegistryError::Store(kbf_state::StoreError::Fenced) => RefusalReason::NotSigner,
                _ => RefusalReason::StoreUnavailable,
            }
        })
    }

    /// The `NodeConfig` of an answer (its generation: [`content_generation`]) for a node that holds `enrolled`'s roles. The cold
    /// store (its URI, with no key in it) goes to a node with the storage role only: a
    /// daemon has no use for it.
    pub(super) fn config(&self, enrolled: &Enrolled, cold: &ColdStoreView) -> pb::NodeConfig {
        let storage = enrolled.certs.iter().any(|cert| cert.role == Role::Storage);
        let config = pb::NodeConfig {
            generation: 0,
            roles: enrolled
                .certs
                .iter()
                .map(|cert| cert.role.name().to_owned())
                .collect(),
            worker_endpoint: self.settings.worker_endpoint.clone(),
            cas_endpoint: self.settings.cas_endpoint.clone(),
            storage_port: self.settings.storage_port,
            cold_store: cold.uri.clone().filter(|_| storage).unwrap_or_default(),
            min_protocol: 0,
            tls_reload_interval_s: TLS_RELOAD_INTERVAL_S,
            frontend_url: String::new(),
            credential_mode: pb::CredentialMode::Keys.into(),
        };
        pb::NodeConfig {
            generation: content_generation(&config),
            ..config
        }
    }

    /// One node certificate for `key`, as the registry records it.
    pub(super) fn issue(
        &self,
        node: &NodeId,
        role: Role,
        key: &PublicKey,
        advertise: Option<&str>,
    ) -> Result<IssuedRole, RefusalReason> {
        let issued = self
            .ca
            .node()
            .issue_node(
                &NodeCert {
                    node_id: node,
                    role,
                    key,
                    advertise,
                },
                self.now(),
                &mut SystemEntropy,
            )
            .map_err(|e| ca_reason(&e))?;
        issued_role(role, key, issued)
    }

    /// A host's updater certificate for `key`, as the registry records it.
    pub(super) fn issue_updater(
        &self,
        node: &NodeId,
        host_role: &HostRole,
        key: &PublicKey,
    ) -> Result<IssuedRole, RefusalReason> {
        let issued = self
            .ca
            .node()
            .issue_updater(
                &UpdaterCert {
                    node_id: node,
                    host_role,
                    key,
                },
                self.now(),
                &mut SystemEntropy,
            )
            .map_err(|e| ca_reason(&e))?;
        issued_role(Role::Updater, key, issued)
    }

    /// The certificate for one CSR's `role`: an updater's with the token's host role, any
    /// other as a node's with `advertise` (a storage node's).
    pub(super) fn issue_for(
        &self,
        node: &NodeId,
        role: Role,
        key: &PublicKey,
        host_role: Option<&HostRole>,
        advertise: Option<&str>,
    ) -> Result<IssuedRole, RefusalReason> {
        match role {
            Role::Updater => {
                self.issue_updater(node, host_role.ok_or(RefusalReason::NotSigner)?, key)
            }
            Role::Storage => self.issue(node, role, key, advertise),
            Role::Daemon => self.issue(node, role, key, None),
        }
    }

    /// The signing step of an `Enroll`: called by the registry once every check has passed.
    fn sign_enroll(
        &self,
        node: &NodeId,
        csrs: &[(Role, &[u8])],
        advertise: &str,
        host_role: Option<&HostRole>,
    ) -> Result<Vec<IssuedRole>, RefusalReason> {
        if node_id_refused(node.as_str(), &self.frontend_names) {
            return Err(RefusalReason::NodeIdNotAllowed);
        }
        let host = advertise
            .to_ascii_lowercase()
            .rsplit_once(':')
            .map(|(host, _)| host.to_owned());
        if host
            .as_deref()
            .is_some_and(|host| advertise_refused(host, &self.frontend_names))
        {
            return Err(RefusalReason::AdvertiseInvalid);
        }
        csrs.iter()
            .map(|(role, der)| {
                let parsed = parse_csr(der).map_err(|_| RefusalReason::BadCsr)?;
                self.issue_for(node, *role, &parsed.key, host_role, host.as_deref())
            })
            .collect()
    }
}

/// The registry's record of `issued`, a certificate for `key`.
fn issued_role(role: Role, key: &PublicKey, issued: Issued) -> Result<IssuedRole, RefusalReason> {
    let time = |t: i64| u64::try_from(t).map_err(|_| RefusalReason::NotSigner);
    Ok(IssuedRole {
        role,
        spki_sha256: Sha256::digest(key.spki()).into(),
        serial: serial_bytes(&issued.serial)?,
        not_before: time(issued.not_before)?,
        not_after: time(issued.not_after)?,
        cert_der: issued.der,
    })
}

/// Runs a call that blocks on the pool and tells its result as a gRPC answer.
async fn run<T: Send + 'static>(
    call: impl FnOnce() -> Result<T, RefusalReason> + Send + 'static,
) -> Result<Response<T>, Status> {
    let done = tokio::task::spawn_blocking(call)
        .await
        .unwrap_or(Err(RefusalReason::StoreUnavailable));
    match done {
        Ok(answer) => Ok(Response::new(answer)),
        Err(reason) => {
            tracing::info!(reason = reason.name(), "enrollment call refused");
            Err(status_of(reason))
        }
    }
}

#[tonic::async_trait]
impl Enroll for EnrollService {
    async fn enroll(
        &self,
        request: Request<pb::EnrollRequest>,
    ) -> Result<Response<pb::EnrollResponse>, Status> {
        let (service, request) = (self.clone(), request.into_inner());
        run(move || service.enroll_now(&request)).await
    }

    async fn renew(
        &self,
        request: Request<pb::RenewRequest>,
    ) -> Result<Response<pb::RenewResponse>, Status> {
        let (service, request) = (self.clone(), request.into_inner());
        run(move || service.renew_now(&request)).await
    }
}

#[cfg(test)]
mod tests;
