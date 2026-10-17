//! Writes: to the first write owner that is not down.

use bytes::Bytes;
use kbf_meta::{ActionRecord, Role};
use kbf_storage::StorageError;
use kbf_types::Digest;

use crate::router::{Router, unavailable, write_order};
use crate::source::MapView;

impl Router {
    /// Sends `bytes` to the first of the blob's write owners that is not `Down` in the map,
    /// once, and answers the digest that node gives back, which must be the digest of the
    /// bytes (hashed here to find the owner). That node answers when it has stored the
    /// blob; the other owners are not written to by this call, and a node that fails is not
    /// followed by a write to the next owner. Whether the node passes the blob on to a
    /// replica is the storage service's (av2-11 in `kbf-storage`), not the router's.
    ///
    /// The bytes are hashed here, and again by the node that stores them.
    ///
    /// # Errors
    /// Every owner of the blob is down; the node's own error.
    pub async fn put(&self, bytes: Bytes) -> Result<Digest, StorageError> {
        let digest = kbf_segments::sha256(&bytes);
        let stored = self
            .routed(|view| self.put_in(view, digest, bytes.clone()))
            .await?;
        if stored != digest {
            return Err(StorageError::Internal(format!(
                "a node stored the blob {digest} as {stored}"
            )));
        }
        Ok(stored)
    }

    /// The second half of an action-cache write: commits the entry for `action` at the
    /// first of the action's write owners that is not `Down`. (The first half, `prepare_action_result`, takes no action
    /// digest and so has no owner to be routed to: the closure check across owners is
    /// av2-12's.)
    ///
    /// # Errors
    /// Every owner of the action is down; the node's refusal or error.
    pub async fn commit_action_record(
        &self,
        role: Role,
        action: Digest,
        record: ActionRecord,
    ) -> Result<(), StorageError> {
        self.routed(|view| self.commit_in(view, role, action, record.clone()))
            .await
    }

    async fn put_in(
        &self,
        view: MapView,
        digest: Digest,
        bytes: Bytes,
    ) -> Result<Digest, StorageError> {
        let primary = primary(&view, &digest)?;
        self.nodes
            .node(&primary, view.map.generation())?
            .put(bytes)
            .await
    }

    async fn commit_in(
        &self,
        view: MapView,
        role: Role,
        action: Digest,
        record: ActionRecord,
    ) -> Result<(), StorageError> {
        let primary = primary(&view, &action)?;
        self.nodes
            .node(&primary, view.map.generation())?
            .commit_action_record(role, action, record)
            .await
    }
}

/// The owner a write of `digest` goes to: the first write owner that is not down.
fn primary(view: &MapView, digest: &Digest) -> Result<kbf_map::Member, StorageError> {
    write_order(&view.map, digest)
        .first()
        .map(|m| (*m).clone())
        .ok_or_else(|| unavailable(format!("every owner of {digest} is down")))
}
