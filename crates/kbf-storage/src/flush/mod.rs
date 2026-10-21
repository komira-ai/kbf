//! One flusher (`docs/design/architecture-v2.md`, section 8): of the owners of a
//! digest, only the primary (rank 1 of [`kbf_map::Map::write_owners`]) PUTs it to the
//! cold store. Otherwise both write the same blobs into different objects and the PUT
//! bytes and the store's cost double. A node that is no owner of a digest at all flushes
//! what it staged of it (an orphan), because no other node holds those staged bytes.
//!
//! [`OneFlusher`] is the node's answer to "do I flush this digest": a [`FlushGate`] the
//! cache asks before it flushes a staged object. It is also the node's [`MapSource`]
//! (it wraps the real one), because that is where a node learns it is behind.
//!
//! - **Rank 1 under the map the node holds.** A `Down` primary keeps its slot in the map
//!   for the dwell, so its replica holds; when the member leaves the map, the replica is
//!   rank 1 under the new map and flushes what the primary never did. The blob is not
//!   lost: the replica's hot copy is still staged.
//! - **An orphan flushes.** A node that is not in [`kbf_map::Map::write_owners`] of a
//!   digest flushes it, whatever its state: a `Draining` member (it takes no writes, so
//!   it owns nothing), a `Joining` one at r = 1 (a joiner owns nothing while a settled
//!   writer exists), a node the map no longer names, or one displaced by a map change.
//!   Nobody else holds what it staged, and the rank-1 owner flushes its own copy of the
//!   same blob too, so the cost of an orphan's flush is a second object of the blob:
//!   keys are disjoint (`cas/<member>/<boot>/<seq>`), so the second PUT overwrites
//!   nothing, and the bytes it adds are bounded by what is staged. A replica (an owner
//!   that is not rank 1) never flushes.
//! - **Fail closed.** A node with no map, or that does not know its own member id, flushes
//!   nothing: a late flush costs hot-store room, a wrong one costs a second PUT.
//! - **A stale map does not make a second flusher.** A caller routed by a generation the
//!   node has not been told of makes the node fetch it ([`crate::replicate`]); that
//!   request names a generation, and until the node holds a map at least that new it
//!   does not flush.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::future::BoxFuture;
use kbf_map::{Map, MemberId};
use kbf_types::Digest;

use crate::replicate::MapSource;

/// Whether this node flushes a digest to the cold store.
pub trait FlushGate: Send + Sync + fmt::Debug {
    /// Whether this node is the one that flushes `digest` now.
    fn flushes(&self, digest: &Digest) -> bool;

    /// The generation of the map this gate judges by, `None` when it holds none or does
    /// not judge by a map (the default). The cache dates the objects it stages by it.
    fn generation(&self) -> Option<u64> {
        None
    }
}

/// The gate of a node that is a member of a farm: it flushes the digests it is rank 1 for,
/// and those it owns no part of.
pub struct OneFlusher {
    source: Arc<dyn MapSource>,
    /// The newest generation any caller has routed by, as far as the node was told.
    newest: AtomicU64,
}

impl fmt::Debug for OneFlusher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OneFlusher")
            .field("newest", &self.newest.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl OneFlusher {
    /// The gate over the node's map source. Give the node this, as its [`MapSource`],
    /// in place of `source`.
    #[must_use]
    pub fn new(source: Arc<dyn MapSource>) -> Arc<Self> {
        Arc::new(Self {
            source,
            newest: AtomicU64::new(0),
        })
    }
}

impl FlushGate for OneFlusher {
    fn flushes(&self, digest: &Digest) -> bool {
        let (Some(map), Some(me)) = (self.source.current(), self.source.me()) else {
            return false;
        };
        map.generation() >= self.newest.load(Ordering::SeqCst) && flushes(&map, me, digest)
    }

    fn generation(&self) -> Option<u64> {
        self.source.current().map(|map| map.generation())
    }
}

/// Whether `me` flushes `digest` under `map`: it is the rank-1 owner, or owns none of it.
/// The owners of a digest are the write owners, so a `Draining` member is never among
/// them.
fn flushes(map: &Map, me: MemberId, digest: &Digest) -> bool {
    let owners = map.write_owners(digest);
    owners.first() == Some(&me) || !owners.contains(&me)
}

impl MapSource for OneFlusher {
    fn current(&self) -> Option<Arc<Map>> {
        self.source.current()
    }

    fn me(&self) -> Option<MemberId> {
        self.source.me()
    }

    fn refetch(&self, at_least: u64) -> BoxFuture<'_, Option<Arc<Map>>> {
        self.newest.fetch_max(at_least, Ordering::SeqCst);
        self.source.refetch(at_least)
    }
}

#[cfg(test)]
mod tests;
