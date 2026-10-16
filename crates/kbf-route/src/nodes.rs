//! The storage nodes a router calls: one [`Storage`] per map member.
//!
//! [`TlsNodes`] dials a member over mutual TLS and verifies it against **its own** name
//! from the map (the host of its address, which is the name its certificate carries),
//! never against one name for every node: a
//! daemon that dialled node-03 and accepted node-02's certificate would send its
//! blobs to whoever holds any certificate of the farm's CA. There is no parameter that
//! sets a name, so there is nothing to override.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use kbf_map::{Member, MemberId};
use kbf_storage::Storage;
use kbf_storage::StorageError;
use kbf_storage::service::RemoteStorage;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

/// How a router reaches a member.
pub trait Nodes: Send + Sync {
    /// The node `member` is, every call of it tagged with map `generation` (the one the
    /// caller routed by, so a node can tell the caller's map is old).
    ///
    /// # Errors
    /// The member cannot be dialled: its address is not one.
    fn node(&self, member: &Member, generation: u64) -> Result<Arc<dyn Storage>, StorageError>;
}

/// Storage nodes over mutual TLS, connected when first used.
#[derive(Debug)]
pub struct TlsNodes {
    /// The CA and this process's identity, and no domain name.
    tls: ClientTlsConfig,
    /// One connection per member address; a member that moves is a new connection.
    channels: Mutex<HashMap<(MemberId, String), Channel>>,
}

impl TlsNodes {
    /// Nodes that present `identity` and trust only `ca`.
    #[must_use]
    pub fn new(ca: Certificate, identity: Identity) -> Self {
        Self {
            tls: ClientTlsConfig::new().ca_certificate(ca).identity(identity),
            channels: Mutex::default(),
        }
    }

    /// The connection to `member`, verified against the host of its address. For a caller
    /// that speaks to a node without a [`Storage`] (the frontend's pass-through of a
    /// ByteStream).
    ///
    /// # Errors
    /// The member's address is not a host and port.
    pub fn channel(&self, member: &Member) -> Result<Channel, StorageError> {
        let key = (member.id, member.addr.clone());
        let mut channels = self.channels.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(channel) = channels.get(&key) {
            return Ok(channel.clone());
        }
        let endpoint = Endpoint::from_shared(format!("https://{}", member.addr))
            .map_err(|e| bad(member, "its address is not a host and port", &e))?;
        // No domain name is set: tonic then verifies the certificate against the host of
        // the endpoint's URI, which is the host of this member's address.
        let channel = endpoint
            .tls_config(self.tls.clone())
            .map_err(|e| bad(member, "its TLS configuration is not usable", &e))?
            .connect_lazy();
        channels.insert(key, channel.clone());
        Ok(channel)
    }
}

impl Nodes for TlsNodes {
    fn node(&self, member: &Member, generation: u64) -> Result<Arc<dyn Storage>, StorageError> {
        Ok(Arc::new(RemoteStorage::new(
            self.channel(member)?,
            generation,
        )))
    }
}

/// A member that cannot be dialled: the map or the configuration is wrong, not the node
/// down.
fn bad(member: &Member, what: &str, error: &dyn std::fmt::Display) -> StorageError {
    StorageError::Internal(format!(
        "member {} at {:?} cannot be dialled, {what}: {error}",
        member.id, member.addr
    ))
}
