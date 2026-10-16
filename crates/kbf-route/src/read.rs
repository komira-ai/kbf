//! Reads (stub of the red-first commit: nothing is routed yet).

use bytes::Bytes;
use kbf_proto::reapi::ActionResult;
use kbf_storage::{ReadStream, StorageError};
use kbf_types::Digest;

use crate::router::{Router, unavailable};

impl Router {
    /// Not routed yet.
    ///
    /// # Errors
    /// Always UNAVAILABLE.
    pub async fn read_range(
        &self,
        _digest: &Digest,
        _offset: u64,
        _limit: u64,
    ) -> Result<ReadStream, StorageError> {
        Err(unavailable("not routed yet".to_owned()))
    }

    /// Not routed yet.
    ///
    /// # Errors
    /// Always UNAVAILABLE.
    pub async fn read_blob(&self, _digest: &Digest) -> Result<Bytes, StorageError> {
        Err(unavailable("not routed yet".to_owned()))
    }

    /// Not routed yet.
    ///
    /// # Errors
    /// Always UNAVAILABLE.
    pub async fn action_result(
        &self,
        _action: &Digest,
    ) -> Result<Option<(Digest, ActionResult)>, StorageError> {
        Err(unavailable("not routed yet".to_owned()))
    }

    pub(crate) async fn open(
        &self,
        _digest: Digest,
        _offset: u64,
        _limit: u64,
    ) -> Result<ReadStream, StorageError> {
        Err(unavailable("not routed yet".to_owned()))
    }
}
