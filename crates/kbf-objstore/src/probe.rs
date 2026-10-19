//! A write, a read and a delete of one small object, to find out whether a key pair
//! works on a bucket before a node builds its cache on it.
//!
//! The object lives under `<root>.kbf-probe/<nonce>`, a prefix nothing else in kbf uses,
//! so a probe that is cut short leaves a stray object nobody lists or reads. The answer
//! is a closed [`ProbeStep`]: the store's own error (an S3 message can quote the request,
//! the bucket or an access key) is not in the result, so a report of a failed probe cannot
//! carry what the store said.

use bytes::Bytes;

use crate::{ByteRange, KeyPrefix, ObjectKey, ObjectStore};

/// The directory under the store's root that probe objects are kept in.
pub const PROBE_DIR: &str = ".kbf-probe/";

/// What the probe writes, and expects back.
const BODY: &[u8] = b"kbf-probe";

/// The step of the probe that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProbeStep {
    /// The nonce does not make a valid object key.
    #[error("the cold-store probe key is not a valid object key")]
    Key,
    /// The write was refused or did not complete.
    #[error("the cold-store probe could not write")]
    Put,
    /// The read failed or returned other bytes than were written.
    #[error("the cold-store probe could not read back what it wrote")]
    Get,
    /// The delete failed: the probe object is still there.
    #[error("the cold-store probe could not delete its object")]
    Delete,
}

/// Writes, reads back and deletes `<root>.kbf-probe/<nonce>` on `store`. `nonce` must be
/// unique to the attempt (A-Z a-z 0-9 - _ .), so a store that refuses to overwrite a key
/// does not refuse the second attempt of a node.
///
/// A failed read still tries the delete, so a failed probe leaves nothing it could have
/// removed.
///
/// # Errors
/// The first [`ProbeStep`] that failed.
pub async fn probe<S: ObjectStore>(
    store: &S,
    root: &KeyPrefix,
    nonce: &str,
) -> Result<(), ProbeStep> {
    let key = ObjectKey::new(format!("{}{PROBE_DIR}{nonce}", root.as_str()))
        .map_err(|_| ProbeStep::Key)?;
    store
        .put_new(&key, Bytes::from_static(BODY), None)
        .await
        .map_err(|_| ProbeStep::Put)?;
    let range = ByteRange::new(0, BODY.len() as u64).ok_or(ProbeStep::Get)?;
    let read = store
        .get_range(&key, range)
        .await
        .ok()
        .filter(|bytes| bytes.as_ref() == BODY);
    let deleted = store.delete(&key).await;
    read.ok_or(ProbeStep::Get)?;
    deleted.map_err(|_| ProbeStep::Delete)
}
