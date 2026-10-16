//! Whether a node is still catching up (`docs/design/architecture-v2.md`, section 8): its
//! index is being rebuilt, so a lookup that finds nothing does not show the blob is not
//! held. While it is, the service answers every such miss UNAVAILABLE ("could not say"),
//! never NOT_FOUND, and a router asks the next owner; hits and writes are served as
//! always. [`CatchUp::finish`] lifts the gate for good.
//!
//! **Why a miss is judged by the state before the lookup.** A call reads
//! [`CatchUp::is_catching_up`] before it asks the store, and judges a miss by that value.
//! Read after, a rebuild that finished between the lookup and the check would turn a miss
//! the half-built index gave into NOT_FOUND. Read before, a `false` means the rebuild had
//! finished (and its index writes are visible, [`Ordering::Acquire`] against the
//! [`Ordering::Release`] of [`CatchUp::finish`]) before the lookup began.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tonic::Status;

/// A node's catch-up state, shared between whatever rebuilds the index (which calls
/// [`CatchUp::finish`]) and the service and registration that read it.
#[derive(Clone, Debug)]
pub struct CatchUp(Arc<AtomicBool>);

impl CatchUp {
    /// A node whose index is being rebuilt: misses are UNAVAILABLE until
    /// [`CatchUp::finish`].
    #[must_use]
    pub fn rebuilding() -> Self {
        Self(Arc::new(AtomicBool::new(true)))
    }

    /// A node whose index is whole: every answer is the store's.
    #[must_use]
    pub fn caught_up() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// Whether the index is still being rebuilt.
    #[must_use]
    pub fn is_catching_up(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// The rebuild is done: from now on a miss is the store's answer. Call it after the
    /// last index write of the rebuild.
    pub fn finish(&self) {
        self.0.store(false, Ordering::Release);
    }
}

/// The answer to a call that found nothing while the node was catching up: UNAVAILABLE,
/// which a caller treats as "this owner could not say" and asks the next.
pub(super) fn unavailable(what: &str) -> Status {
    Status::unavailable(format!(
        "catching up: this node is rebuilding its index, so {what} may be held but not yet \
         indexed; ask another owner or try again"
    ))
}
