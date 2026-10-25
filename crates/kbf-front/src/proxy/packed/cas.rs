//! The batch CAS calls over the packs: FindMissingBlobs, BatchUpdateBlobs and
//! BatchReadBlobs of the proxy's `ContentAddressableStorage`, each asking the packs first and
//! the storage nodes for what the packs do not serve.

use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use kbf_proto::reapi::{self, batch_update_blobs_response};
use kbf_types::Digest;
use tonic::Status;

use super::Packs;
use crate::proxy::Owners;
use crate::proxy::cas::checked;
use crate::wire;

/// One item of a BatchUpdateBlobs: its digest as sent, its compressor and its data.
pub(in crate::proxy) type Item = (Option<reapi::Digest>, i32, Bytes);

/// The digests among `asked` that neither the packs nor the storage nodes hold. The index is
/// asked about all of them, and the storage nodes only about the ones it lacks, so a call
/// whose blobs are all in packs asks no storage node.
///
/// # Errors
/// UNAVAILABLE when the index could not answer, or as the storage nodes answer.
pub(in crate::proxy) async fn find_missing(
    packs: &Packs,
    owners: &Owners,
    asked: &[Digest],
) -> Result<BTreeSet<Digest>, Status> {
    let absent = packs.missing(asked).await?;
    if absent.is_empty() {
        return Ok(BTreeSet::new());
    }
    owners.find_missing(&absent).await
}

/// What the packs answered for each digest of a BatchReadBlobs that is a digest, by digest.
pub(in crate::proxy) type Served = BTreeMap<Digest, Result<Option<Bytes>, Status>>;

/// The packs' answer for each of `asked` that is a digest, read together (the index is asked
/// once for the batch, and blobs close together in a pack are one request). An item that is
/// not a digest has no answer here; the caller refuses it alone.
pub(in crate::proxy) async fn read_many(packs: &Packs, asked: &[reapi::Digest]) -> Served {
    let valid: Vec<Digest> = asked
        .iter()
        .filter_map(|d| wire::digest(Some(d)).ok())
        .collect();
    let found = packs.read_many(&valid).await;
    valid.into_iter().zip(found).collect()
}

/// The blob `digest`: as the packs answered for it in `served` if they serve it, else from the
/// first storage node that has it.
///
/// # Errors
/// As the packs or the storage nodes answer.
pub(in crate::proxy) async fn read_blob(
    served: &Served,
    owners: &Owners,
    digest: &Digest,
) -> Result<Bytes, Status> {
    let found = served.get(digest).cloned().unwrap_or(Ok(None));
    match found? {
        Some(bytes) => Ok(bytes),
        None => owners.read_blob(digest).await,
    }
}

/// Packs the blobs of `items` that pass the checks of a stored blob, together, and answers
/// each item alone, in order: the refusal of its own check, or, for a blob that passed, OK
/// when the packs hold it and the index has its row, else the failure of the write. The bytes
/// of those blobs are taken from the room that uploads share first ([`Packs::put`]), waiting
/// for it if need be; when they are together over all of it, each of them is RESOURCE_EXHAUSTED.
pub(in crate::proxy) async fn store_batch(
    packs: &Packs,
    items: Vec<Item>,
) -> Vec<batch_update_blobs_response::Response> {
    let verified: Vec<_> = items
        .into_iter()
        .map(|(digest, compressor, data)| {
            let blob = checked(digest.as_ref(), compressor, data);
            (digest, blob)
        })
        .collect();
    let good: Vec<(Digest, Bytes)> = verified
        .iter()
        .filter_map(|(_, blob)| blob.as_ref().ok().cloned())
        .collect();
    let stored = packs.put(good).await;
    verified
        .into_iter()
        .map(|(digest, blob)| {
            let refusal = blob.err().or_else(|| stored.clone().err());
            let status = refusal.map_or_else(wire::rpc_ok, |status| wire::rpc_status(&status));
            batch_update_blobs_response::Response {
                digest,
                status: Some(status),
            }
        })
        .collect()
}
