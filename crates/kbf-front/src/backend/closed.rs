//! The closure check of the action cache when the frontend keeps blobs in packs.
//!
//! The nodes check an action-cache entry against the blobs they hold themselves, and a blob a
//! daemon wrote into a pack is not on a node. A frontend given [`Packs`] therefore works the
//! closure out itself, on every write and on every hit, and counts a blob as held when the pack
//! index has it ([`Layered`](super::layered::Layered): the index first, the nodes for the
//! rest):
//!
//! - **The write.** [`checked`] works the closure of a result out (reading its output trees from
//!   the packs or the nodes) and refuses the result unless every blob in it is held. The commit
//!   then hands the nodes the entry with only the blobs they hold themselves ([`on_nodes`]), as
//!   they check an entry against their own index and would refuse a blob that is only in a pack.
//! - **The hit.** [`served`] works the closure of the result of a hit out again and turns the
//!   hit into a miss if a blob in it is held by neither the index nor the nodes. The entry's own
//!   closure is not used, as it names only what the nodes hold.
//!
//! Nothing here runs without packs: the backend then asks the nodes alone, as it always did.

use kbf_meta::{ActionRecord, ActionWriteError, Closure};
use kbf_proto::reapi::ActionResult;
use kbf_storage::StorageError;
use kbf_storage::service::MAX_FIND_MISSING_DIGESTS;
use kbf_types::Digest;

use super::Answer;
use super::layered::unavailable;
use crate::closure;
use crate::input_walk::Blobs;
use crate::proxy::Packs;

/// The closure of `result` and the blobs of it that `blobs` does not hold.
///
/// # Errors
/// A malformed result, a tree that cannot be read or decoded, or `blobs` could not say.
async fn lacking(result: &ActionResult, blobs: &dyn Blobs) -> Answer<(Closure, Vec<Digest>)> {
    let (mut closure, trees) = closure::direct(result)?;
    for tree in trees {
        let bytes = blobs.read_blob(&tree).await?;
        closure::add_tree(&mut closure, &tree, bytes)?;
    }
    let needed: Vec<Digest> = closure.iter().copied().collect();
    let mut missing = Vec::new();
    // A node takes at most this many digests in one call.
    for asked in needed.chunks(MAX_FIND_MISSING_DIGESTS) {
        missing.extend(blobs.find_missing(asked).await?);
    }
    Ok((closure, missing))
}

/// The closure of `result`, if every blob in it is held by `blobs`.
///
/// # Errors
/// [`ActionWriteError::Absent`] naming every blob of the closure `blobs` does not hold; as
/// [`lacking`].
pub(crate) async fn checked(result: &ActionResult, blobs: &dyn Blobs) -> Answer<Closure> {
    let (closure, missing) = lacking(result, blobs).await?;
    if missing.is_empty() {
        return Ok(closure);
    }
    Err(ActionWriteError::Absent(missing).into())
}

/// `hit` if every blob its result names is held by `blobs`; a miss if one is not, or a tree of
/// the result is not there or does not decode.
///
/// # Errors
/// `blobs` could not say.
pub(crate) async fn served(
    hit: Option<(Digest, ActionResult)>,
    blobs: &dyn Blobs,
) -> Answer<Option<(Digest, ActionResult)>> {
    let Some((_, result)) = &hit else {
        return Ok(None);
    };
    let looked = lacking(result, blobs).await;
    match looked {
        Ok((_, missing)) if missing.is_empty() => Ok(hit),
        Ok(_) | Err(StorageError::NotFound(_) | StorageError::Invalid(_)) => Ok(None),
        Err(other) => Err(other),
    }
}

/// `record` with only the blobs of its closure that no pack holds: the ones a node holds itself,
/// which is all a node checks an entry against.
///
/// # Errors
/// The pack index could not say.
pub(crate) async fn on_nodes(packs: &Packs, record: ActionRecord) -> Answer<ActionRecord> {
    let wanted: Vec<Digest> = record.closure.iter().copied().collect();
    let unpacked = packs.missing(&wanted).await.map_err(unavailable)?;
    Ok(ActionRecord {
        result: record.result,
        closure: unpacked.into_iter().collect(),
    })
}
