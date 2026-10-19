//! `kbf.enroll.v1.ColdStore` on the worker listener: a storage node fetches the farm's
//! registered cold store and its keys.
//!
//! **Who may call.** The listener's role table admits the storage role alone
//! ([`crate::worker_roles`]); the service checks it again, as `Membership` does, and then,
//! at every call, never once per connection:
//!
//! - the worker listener's deny list file (a listed serial, key or node, or an unusable
//!   list, which refuses everything);
//! - the operator's `deny/` rows ([`Members::denied`]), which revocation writes, so a node
//!   revoked while its session is open is refused at its next call;
//! - when the frontend keeps an enrollment registry, that the node (the certificate's first
//!   DNS name, the node id enrollment gives it) is an `Active` row in it.
//!
//! Behind the reverse proxy there is no certificate: the identity is the node id the proxy
//! forwarded, so only a `node` deny entry (or a `deny/` row naming the node) can match it, never
//! a serial or a key. `with_registry` (enrollment revocation) is not wired in the frontend until
//! the enroll listener lands.
//!
//! Without the registry only the first two apply, so a frontend that does not run
//! enrollment still refuses a denied node. **Plain text refuses**: under
//! [`Admission::Open`] (no worker TLS) the listener's role table lets every call through
//! and a caller has no role, so the one RPC that carries the cold store's secret is
//! PERMISSION_DENIED with fixed text rather than served in clear to whoever reaches the port.
//!
//! **What it answers.** One [`Snapshot`] of the registration. If the node already holds
//! the snapshot's generation (`have_generation` equals it), or nothing is registered, the
//! answer is the bare generation and the secret is dropped here, whatever the source
//! returned. Any other `have_generation`, a lower one too, gets the whole store.
//!
//! **Failures** are fixed text. UNAVAILABLE for everything the node should retry (the
//! secret is not readable, the state store failed); PERMISSION_DENIED for a caller that may not ask. No path, row, error of the store or
//! byte of the secret is in a status.

use std::sync::Arc;

use kbf_enroll_registry::{NodeState, node_state};
use kbf_proto::kbf::enroll::v1 as pb;
use kbf_proto::kbf::enroll::v1::cold_store_server::{ColdStore, ColdStoreServer};
use kbf_state::Store;
use kbf_storage::service::{Admission, PeerRole, role_of};
use kbf_types::NodeId;
use tonic::{Request, Response, Status};

use super::{SourceError, StoreConfigSource};
use crate::identity::{DenyList, PeerCert, Presented, presented};
use crate::members::Members;
use crate::members_rpc::refuse_denied;

/// The `ColdStore` service over a registration `source`.
pub struct ColdStoreService {
    source: Arc<dyn StoreConfigSource>,
    admission: Admission,
    deny_list: Option<DenyList>,
    denied_rows: Option<Arc<Members>>,
    registry: Option<Arc<dyn Store>>,
}

impl ColdStoreService {
    /// The service over `source`, admitting callers as `admission` says, with no deny list,
    /// no `deny/` rows and no registry.
    #[must_use]
    pub fn new(source: Arc<dyn StoreConfigSource>, admission: Admission) -> Self {
        Self {
            source,
            admission,
            deny_list: None,
            denied_rows: None,
            registry: None,
        }
    }

    /// The service refusing the certificates and nodes `deny_list` lists, read again at
    /// every call.
    #[must_use]
    pub fn with_deny_list(mut self, deny_list: Option<DenyList>) -> Self {
        self.deny_list = deny_list;
        self
    }

    /// The service refusing what the operator's `deny/` rows (kept by `members`) name,
    /// read again at every call.
    #[must_use]
    pub fn with_denied_rows(mut self, members: Option<Arc<Members>>) -> Self {
        self.denied_rows = members;
        self
    }

    /// The service answering only nodes that are `Active` in the enrollment registry held
    /// in `registry`, read again at every call.
    #[must_use]
    pub fn with_registry(mut self, registry: Option<Arc<dyn Store>>) -> Self {
        self.registry = registry;
        self
    }

    /// The service as a tonic server, ready to add to a listener.
    #[must_use]
    pub fn server(self) -> ColdStoreServer<Self> {
        ColdStoreServer::new(self)
    }

    /// Admits a caller whose verified chain starts with `leaf`, by what the module docs
    /// list; its certificate, or `None` without mutual TLS. A certificate caller of
    /// [`Self::admit_presented`].
    ///
    /// # Errors
    /// As [`Self::admit_presented`].
    pub async fn admit_leaf(&self, leaf: Option<&[u8]>) -> Result<Option<PeerCert>, Status> {
        self.admit_presented(&Presented::Leaf(leaf.map(<[u8]>::to_vec)))
            .await
    }

    /// Admits a caller that presents `presented` ([`crate::identity::presented`], the one
    /// function that says who is calling): the node and role a proxy forwarded under its
    /// secret, else the client certificate. Then the checks of the module docs run on the
    /// node it names; a proxied identity names one node and has no serial or key, so only a
    /// `node` entry of the deny list can name it.
    ///
    /// # Errors
    /// UNAUTHENTICATED: under mutual TLS, no certificate or one that does not parse, or a
    /// proxied identity with no role or node. PERMISSION_DENIED: plain text
    /// ([`Admission::Open`]), not the storage role, denied, or not an active node of the
    /// registry. UNAVAILABLE: the deny list or a state store cannot be read.
    pub async fn admit_presented(&self, presented: &Presented) -> Result<Option<PeerCert>, Status> {
        if self.admission == Admission::Open {
            return Err(Status::permission_denied(
                "the cold store is served over mutual TLS only",
            ));
        }
        let peer = match presented {
            Presented::Proxy(identity) => match (identity.role, identity.node_id.as_deref()) {
                (Some(PeerRole::Storage), Some(node)) => PeerCert::proxied(node),
                (Some(PeerRole::Storage) | None, _) => {
                    return Err(Status::unauthenticated(
                        "the proxy forwarded no node id and storage role",
                    ));
                }
                (Some(other), _) => return Err(not_storage(other)),
            },
            Presented::Leaf(_) => {
                let der = presented
                    .leaf()
                    .ok_or_else(|| Status::unauthenticated("no client certificate"))?;
                match role_of(der)? {
                    PeerRole::Storage => {
                        PeerCert::from_der(der).map_err(Status::unauthenticated)?
                    }
                    other => return Err(not_storage(other)),
                }
            }
        };
        if let Some(deny_list) = &self.deny_list {
            deny_list.admit_cert(&peer).await?;
        }
        let members = self.denied_rows.clone();
        let registry = self.registry.clone();
        let checked = peer.clone();
        on_blocking(move || {
            if let Some(members) = &members {
                // Fixed text: the store's own error is not for a node.
                let denied = members
                    .denied()
                    .map_err(|_| Status::unavailable("the denied nodes could not be read"))?;
                refuse_denied(&checked, &denied)
                    .map_err(|e| Status::permission_denied(e.to_string()))?;
            }
            Ok(())
        })
        .await?;
        if let Some(registry) = registry {
            let node = peer.identity().names().dns().first().cloned();
            on_blocking(move || active(registry.as_ref(), node.as_deref())).await?;
        }
        Ok(Some(peer))
    }

    /// The answer for a caller already admitted, holding `have` (see the module docs).
    ///
    /// # Errors
    /// UNAVAILABLE: [`SourceError`], as fixed text.
    pub async fn answer(&self, have: u64) -> Result<pb::GetStoreConfigResponse, Status> {
        let source = Arc::clone(&self.source);
        let snapshot = on_blocking(move || source.snapshot().map_err(unavailable)).await?;
        let unchanged = snapshot.config.is_none() || snapshot.generation == have;
        Ok(pb::GetStoreConfigResponse {
            generation: snapshot.generation,
            config: if unchanged { None } else { snapshot.config },
        })
    }
}

/// Runs `call` on a blocking thread (the state store and the secret file block). A call
/// that panics is INTERNAL with fixed text: nothing of the panic reaches the node.
async fn on_blocking<T: Send + 'static>(
    call: impl FnOnce() -> Result<T, Status> + Send + 'static,
) -> Result<T, Status> {
    tokio::task::spawn_blocking(call)
        .await
        .unwrap_or_else(|_| Err(Status::internal("the store config call failed")))
}

/// Whether the registry holds `node` as `Active`.
fn active(registry: &dyn Store, node: Option<&str>) -> Result<(), Status> {
    let id = node
        .and_then(|name| name.parse::<NodeId>().ok())
        .ok_or_else(|| Status::permission_denied("the certificate names no enrolled node"))?;
    match node_state(registry, &id) {
        Ok(Some(NodeState::Active)) => Ok(()),
        Ok(_) => Err(Status::permission_denied(
            "the node is not enrolled or is revoked",
        )),
        Err(_) => Err(Status::unavailable(
            "the enrollment registry cannot be read",
        )),
    }
}

/// The refusal of a caller whose role is not `storage`.
fn not_storage(role: PeerRole) -> Status {
    Status::permission_denied(format!("role {} may not fetch the cold store", role.name()))
}

/// The status for a source that cannot answer: retry, with fixed text.
fn unavailable(e: SourceError) -> Status {
    Status::unavailable(e.to_string())
}

#[tonic::async_trait]
impl ColdStore for ColdStoreService {
    async fn get_store_config(
        &self,
        request: Request<pb::GetStoreConfigRequest>,
    ) -> Result<Response<pb::GetStoreConfigResponse>, Status> {
        self.admit_presented(&presented(request.extensions()))
            .await?;
        let have = request.into_inner().have_generation;
        self.answer(have).await.map(Response::new)
    }
}
