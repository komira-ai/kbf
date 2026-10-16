//! Writes: to the primary owner; the chain forwards to the replica (av2-11).

use bytes::Bytes;
use kbf_meta::{ActionRecord, Role};
use kbf_storage::StorageError;
use kbf_types::Digest;

use crate::router::{Router, unavailable, write_order};
use crate::source::MapView;

impl Router {
    /// Stores `bytes` and answers their digest once the primary owner holds them
    /// durably. The blob goes to the primary only, once; the primary forwards it to the
    /// replica and answers when both hold it (av2-11), so a client of a 1 GbE link sends
    /// it once, not R times. A primary that is `Down` in the map is skipped: the write
    /// goes to the next owner, which is not down.
    ///
    /// The bytes are hashed here to find their owner, and again by the node that stores
    /// them.
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
    /// action's primary owner. (The first half, `prepare_action_result`, takes no action
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

/// The owner a write of `digest` goes to.
fn primary(view: &MapView, digest: &Digest) -> Result<kbf_map::Member, StorageError> {
    write_order(&view.map, digest)
        .first()
        .map(|m| (*m).clone())
        .ok_or_else(|| unavailable(format!("every owner of {digest} is down")))
}
