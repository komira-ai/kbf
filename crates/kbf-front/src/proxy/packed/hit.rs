//! The closure check of an action-cache hit that a client asks for, when the frontend keeps
//! blobs in packs.
//!
//! The storage node that answers a `GetActionResult` checks the blobs of its entry's closure
//! that it holds itself; a blob that a daemon wrote into a pack is not among them (the entry a
//! frontend commits names only the blobs the nodes hold, `crate::Backend`). So the hit is
//! checked here, as every hit is: the closure of its result is worked out again, and every
//! blob in it must be in the pack index or on the nodes. A result with a blob that is in neither
//! is a miss, and the client runs the action again.
//!
//! On a routed farm the nodes are asked as `FindMissingBlobs` asks them: each blob of its
//! primary only. The check of an Execute also tries the other owners of a blob, so for a while
//! after the map changes a blob that only a previous owner holds is held there and missing here:
//! the client's lookup is a miss and it runs the action again. A hit is never wrongly served.

use std::sync::Arc;

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_proto::reapi::ActionResult;
use kbf_storage::StorageError;
use kbf_types::Digest;
use tonic::{Code, Status};

use super::Packs;
use crate::backend::{Layered, served};
use crate::input_walk::Blobs;
use crate::proxy::owners::Owners;

type Answer<T> = Result<T, StorageError>;

/// `hit`, an entry the storage nodes answered for, if the index or the nodes hold everything its
/// result names; else a miss.
///
/// # Errors
/// UNAVAILABLE when the index or the nodes could not say.
pub(in crate::proxy) async fn held(
    packs: &Packs,
    owners: &Owners,
    hit: Option<(Digest, ActionResult)>,
) -> Result<Option<(Digest, ActionResult)>, Status> {
    let nodes = Nodes(owners);
    let layered = Layered {
        packs,
        rest: &nodes,
    };
    Ok(served(hit, &layered).await?)
}

/// The storage nodes, as the closure check reads them.
struct Nodes<'a>(&'a Owners);

impl Blobs for Nodes<'_> {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Answer<Bytes>> {
        Box::pin(async move {
            let read = self.0.read_blob(digest).await;
            read.map_err(|status| refused(status, digest))
        })
    }

    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> BoxFuture<'a, Answer<Vec<Digest>>> {
        Box::pin(async move {
            let missing = self.0.find_missing(digests).await.map_err(unavailable)?;
            let absent = digests.iter().filter(|d| missing.contains(*d));
            Ok(absent.copied().collect())
        })
    }
}

/// The nodes' refusal to read `digest` as the check reads it: not found if they hold no such
/// blob, else unavailable.
fn refused(status: Status, digest: &Digest) -> StorageError {
    match status.code() {
        Code::NotFound => StorageError::NotFound(*digest),
        _ => unavailable(status),
    }
}

/// A call to the nodes that did not answer.
fn unavailable(status: Status) -> StorageError {
    StorageError::Unavailable(Arc::new(status))
}

#[cfg(test)]
mod tests {
    use kbf_objstore::{Capabilities, MemoryStore};
    use kbf_pack::{PackKeys, WriterKind};
    use kbf_pack_index::PackIndex;
    use kbf_pack_index::memory::MemoryPackIndex;
    use kbf_proto::reapi::{OutputDirectory, OutputFile};
    use kbf_state::ManualClock;
    use tonic::transport::Endpoint;

    use super::*;
    use crate::digest_to_proto;
    use crate::proxy::{Owner, PackSettings};

    /// Packs over an empty store and an empty index.
    fn packs() -> Packs {
        let create_only = Capabilities {
            conditional_put: true,
            object_lock: false,
        };
        let store = Arc::new(MemoryStore::new(create_only));
        let clock = Arc::new(ManualClock::default());
        let index: Arc<dyn PackIndex> = Arc::new(MemoryPackIndex::new(clock));
        let keys = PackKeys::new(WriterKind::Frontend, "fe1", "boot1").expect("keys");
        Packs::new(store, index, keys, PackSettings::default()).expect("packs")
    }

    /// A storage node that nobody listens at.
    fn down() -> Owners {
        let channel = Endpoint::from_static("http://127.0.0.1:1").connect_lazy();
        Owners::single(Owner::new("down", channel))
    }

    /// An entry of the action cache whose result is `result`.
    fn entry(result: ActionResult) -> Option<(Digest, ActionResult)> {
        Some((kbf_segments::sha256(b"the result"), result))
    }

    /// Asserts that a lookup that found no entry is a miss without asking the nodes, which are
    /// not there to be asked.
    #[tokio::test]
    async fn no_entry_is_a_miss_without_asking_the_nodes() {
        let answered = held(&packs(), &down(), None).await;

        assert!(answered.expect("answered").is_none());
    }

    /// Asserts that nodes that cannot be reached are not taken for holding the blobs of a hit:
    /// whether the check asks them which output files they hold or reads an output tree from
    /// them, the lookup fails UNAVAILABLE and is neither a hit nor a miss.
    #[tokio::test]
    async fn nodes_that_cannot_be_reached_fail_the_check_of_a_hit() {
        let output = kbf_segments::sha256(b"an output");
        let file = ActionResult {
            output_files: vec![OutputFile {
                path: "f".to_owned(),
                digest: Some(digest_to_proto(&output)),
                ..OutputFile::default()
            }],
            ..ActionResult::default()
        };
        let tree = ActionResult {
            output_directories: vec![OutputDirectory {
                path: "d".to_owned(),
                tree_digest: Some(digest_to_proto(&output)),
                ..OutputDirectory::default()
            }],
            ..ActionResult::default()
        };

        for result in [file, tree] {
            let answered = held(&packs(), &down(), entry(result)).await;

            let refused = answered.expect_err("the nodes cannot say");
            assert_eq!(refused.code(), Code::Unavailable);
        }
    }
}
