//! Writes (stub of the red-first commit: nothing is routed yet).

use bytes::Bytes;
use kbf_meta::{ActionRecord, Role};
use kbf_storage::StorageError;
use kbf_types::Digest;

use crate::router::{Router, unavailable};

impl Router {
    /// Not routed yet.
    ///
    /// # Errors
    /// Always UNAVAILABLE.
    pub async fn put(&self, _bytes: Bytes) -> Result<Digest, StorageError> {
        Err(unavailable("not routed yet".to_owned()))
    }

    /// Not routed yet.
    ///
    /// # Errors
    /// Always UNAVAILABLE.
    pub async fn commit_action_record(
        &self,
        _role: Role,
        _action: Digest,
        _record: ActionRecord,
    ) -> Result<(), StorageError> {
        Err(unavailable("not routed yet".to_owned()))
    }
}
