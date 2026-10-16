//! The frontend as a proxy to storage nodes (`docs/design/architecture-v2.md`, sections
//! 2, 7 and 9): a client's CAS, ByteStream and action-cache calls are answered by a
//! storage node (`kbf storage`), not by this process's cache. Behind `--storage` of
//! `kbf-server`; without it nothing here runs.
//!
//! **Two levels.** ByteStream is a raw HTTP/2 pass-through ([`bytestream`]): single
//! blobs of any size cross the frontend in constant memory, and nothing here caps their
//! size. The batch CAS calls and the action cache are tonic services ([`cas`], [`ac`])
//! that decode their messages, which are bounded by the gRPC message limit, to split
//! them by owner.
//!
//! **Routing** is [`Owners::for_digest`], the one place that maps a digest to the nodes
//! that hold it. It answers one fixed list today (a farm of one node); routing by digest
//! replaces it alone.
//!
//! **Authorization** is the cache's (`kbf_auth::Authorizers`): the tonic services ask it
//! as the cache's services do, and the raw path asks the same authorizer, of the
//! instance name in the peeked resource name, with the caller the HTTP-level
//! authentication layer found.
//!
//! **Not here yet:** the scheduler's use of storage (Execute still reads the local
//! cache), health and readiness (av2-04), mutual TLS to the nodes, GetTree.

mod ac;
mod body;
mod bytestream;
mod cas;
mod meter;
mod name;
mod owners;

use std::sync::Arc;

use kbf_auth::Authorizers;
use kbf_proto::reapi::action_cache_server::ActionCacheServer;
use kbf_proto::reapi::content_addressable_storage_server::ContentAddressableStorageServer;
use tonic::service::Routes;
use tonic::transport::Endpoint;

pub use self::meter::{Direction, Meter, Method};
pub use self::owners::{Owner, Owners};
use self::{ac::ProxyActionCache, bytestream::RawByteStream, cas::ProxyCas};
use crate::MAX_MESSAGE_BYTES;

/// The proxy: which nodes, who may call, and what has passed.
#[derive(Clone, Debug)]
pub struct Proxy {
    owners: Owners,
    authorizers: Arc<Authorizers>,
    meter: Arc<Meter>,
}

impl Proxy {
    /// A proxy to `owners`, each call authorized by `authorizers`.
    #[must_use]
    pub fn new(owners: Owners, authorizers: Arc<Authorizers>) -> Self {
        Self {
            owners,
            authorizers,
            meter: Arc::default(),
        }
    }

    /// A proxy to the one storage node at `endpoint`, connected when first used.
    #[must_use]
    pub fn to_endpoint(endpoint: &Endpoint, authorizers: Arc<Authorizers>) -> Self {
        let node = Owner::new(&endpoint.uri().to_string(), endpoint.connect_lazy());
        Self::new(Owners::single(node), authorizers)
    }

    /// What the raw path has counted.
    #[must_use]
    pub fn meter(&self) -> &Arc<Meter> {
        &self.meter
    }

    /// `ContentAddressableStorage`, `ByteStream` and `ActionCache`.
    #[must_use]
    pub fn routes(&self) -> Routes {
        let cas = ProxyCas {
            owners: self.owners.clone(),
            authorizers: Arc::clone(&self.authorizers),
        };
        let action_cache = ProxyActionCache {
            owners: self.owners.clone(),
            authorizers: Arc::clone(&self.authorizers),
        };
        let bytestream = RawByteStream {
            owners: self.owners.clone(),
            authorizers: Arc::clone(&self.authorizers),
            meter: Arc::clone(&self.meter),
        };
        Routes::new(
            ContentAddressableStorageServer::new(cas)
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        )
        .add_service(bytestream)
        .add_service(ActionCacheServer::new(action_cache))
    }
}
