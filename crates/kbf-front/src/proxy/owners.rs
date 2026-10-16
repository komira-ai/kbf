//! Which storage node owns a blob. Today the answer is one node for every digest; the
//! routing by digest of architecture-v2 section 8 replaces [`Owners::for_digest`] and
//! nothing else (av2-08 and av2-09).

use std::sync::{Arc, RwLock};

use kbf_storage::service::RemoteStorage;
use kbf_types::Digest;
use tonic::Status;
use tonic::transport::Channel;

/// One storage node: a name for logs and for telling two apart, and the connection.
#[derive(Clone, Debug)]
pub struct Owner {
    id: Arc<str>,
    pub(super) channel: Channel,
    pub(super) storage: RemoteStorage,
}

impl Owner {
    /// The node `channel` reaches, called `id`.
    #[must_use]
    pub fn new(id: &str, channel: Channel) -> Self {
        Self {
            id: Arc::from(id),
            storage: RemoteStorage::new(channel.clone(), 0),
            channel,
        }
    }

    /// The node's name.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// The nodes a frontend calls, in the order a digest asks them. Cheap to clone; every
/// clone sees a [`replace`](Owners::replace).
#[derive(Clone, Debug)]
pub struct Owners {
    nodes: Arc<RwLock<Arc<[Owner]>>>,
}

impl Owners {
    /// A farm of one node.
    #[must_use]
    pub fn single(owner: Owner) -> Self {
        Self::ranked(vec![owner])
    }

    /// These nodes, first the one a digest asks first. Until routing by digest exists,
    /// every digest asks them in this order.
    #[must_use]
    pub fn ranked(nodes: Vec<Owner>) -> Self {
        Self {
            nodes: Arc::new(RwLock::new(nodes.into())),
        }
    }

    /// The nodes now, as a map change makes them. A call in flight keeps the nodes it
    /// asked for, except an upload, which ends UNAVAILABLE if its primary is no longer
    /// first.
    pub fn replace(&self, nodes: Vec<Owner>) {
        // A poisoned lock still holds a whole list: nobody writes half of one.
        *self
            .nodes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = nodes.into();
    }

    /// The nodes that hold `digest`, the primary first, then the replicas in the order
    /// a read tries them. THE ROUTING SEAM: av2-09 replaces this body with the map's
    /// answer for the digest.
    #[must_use]
    pub fn for_digest(&self, _digest: &Digest) -> Arc<[Owner]> {
        Arc::clone(
            &self
                .nodes
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// The node that stores `digest` first, or UNAVAILABLE when the farm has none.
    pub(super) fn primary(&self, digest: &Digest) -> Result<Owner, Status> {
        self.for_digest(digest)
            .first()
            .cloned()
            .ok_or_else(no_storage)
    }
}

/// The answer when no node holds a blob.
pub(super) fn no_storage() -> Status {
    Status::unavailable("the frontend has no storage node for this blob")
}
