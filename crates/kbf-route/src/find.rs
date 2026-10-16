//! FindMissing (stub of the red-first commit: nothing is routed yet).

use kbf_storage::StorageError;
use kbf_types::Digest;

use crate::router::{Router, unavailable};

impl Router {
    /// Not routed yet.
    ///
    /// # Errors
    /// Always UNAVAILABLE.
    pub async fn find_missing(&self, _digests: &[Digest]) -> Result<Vec<Digest>, StorageError> {
        Err(unavailable("not routed yet".to_owned()))
    }
}
