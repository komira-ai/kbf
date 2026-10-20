//! Read repair: what a router does when a read finds a blob somewhere other than where
//! it should be.
//!
//! A read that is answered by an owner after an earlier owner of the same generation
//! said it did not hold the blob has found a copy missing. The router tells a
//! [`ReadRepair`] hook (the repair side, av2-11, implements it) which node answered and
//! which owners lacked the blob, and goes on serving the read: the hook only queues. The
//! router rate-limits the hook ([`RepairLimit`]) because a cold owner during a join
//! answers NOT_FOUND for most of what it is asked, and every such read would otherwise
//! be a repair.

use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, PoisonError};

use kbf_map::MemberId;
use kbf_types::Digest;
use tokio::sync::Semaphore;
use tokio::time::Instant;

use crate::router::Router;

/// The repair side's entry point.
pub trait ReadRepair: Send + Sync {
    /// A read of `digest` was answered by `found_at` after the owners `missing_at`, of
    /// the map the read was routed by, answered that they did not hold it (or held it
    /// unreadably, or sent bytes that are not the blob). Returns at once: it queues, it never copies the blob inline.
    fn found_elsewhere(&self, digest: &Digest, found_at: MemberId, missing_at: &[MemberId]);
}

/// How many times the hook may be called: `burst` at once, then `per_second`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RepairLimit {
    /// The most calls that may come together.
    pub burst: NonZeroU32,
    /// How many calls the limit gives back each second.
    pub per_second: NonZeroU32,
}

/// A token bucket on the runtime's clock (so a test with a paused clock decides it).
#[derive(Debug)]
pub(crate) struct Limiter {
    limit: RepairLimit,
    state: Mutex<Bucket>,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Limiter {
    pub(crate) fn new(limit: RepairLimit) -> Self {
        Self {
            limit,
            state: Mutex::new(Bucket {
                tokens: f64::from(limit.burst.get()),
                at: Instant::now(),
            }),
        }
    }

    /// Whether a call may go ahead now; takes its token if so.
    pub(crate) fn allow(&self) -> bool {
        let now = Instant::now();
        let mut bucket = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let elapsed = now.saturating_duration_since(bucket.at).as_secs_f64();
        bucket.at = now;
        let full = f64::from(self.limit.burst.get());
        bucket.tokens =
            (bucket.tokens + elapsed * f64::from(self.limit.per_second.get())).min(full);
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            return true;
        }
        false
    }
}

/// The most blobs [`Router::with_background_repair`] copies at once.
pub const MAX_REPAIRS_IN_FLIGHT: usize = 4;

/// The router's own [`ReadRepair`]: copies the blob to the owners that lacked it, one task
/// per digest, never inline.
pub(crate) struct Background {
    shared: Arc<Shared>,
}

struct Shared {
    /// A router without a repair hook.
    router: Router,
    slots: Arc<Semaphore>,
    /// The digests being copied now.
    active: Mutex<BTreeSet<Digest>>,
}

/// Takes a digest out of the set of those being copied when its task ends, however it ends.
struct Active {
    shared: Arc<Shared>,
    digest: Digest,
}

impl Drop for Active {
    fn drop(&mut self) {
        self.shared
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.digest);
    }
}

impl Background {
    pub(crate) fn new(router: Router) -> Self {
        Self {
            shared: Arc::new(Shared {
                router,
                slots: Arc::new(Semaphore::new(MAX_REPAIRS_IN_FLIGHT)),
                active: Mutex::default(),
            }),
        }
    }
}

impl ReadRepair for Background {
    fn found_elsewhere(&self, digest: &Digest, _found_at: MemberId, missing_at: &[MemberId]) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let Ok(permit) = Arc::clone(&self.shared.slots).try_acquire_owned() else {
            return;
        };
        let fresh = self
            .shared
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(*digest);
        if !fresh {
            return;
        }
        let active = Active {
            shared: Arc::clone(&self.shared),
            digest: *digest,
        };
        let missing = missing_at.to_vec();
        runtime.spawn(async move {
            active.shared.repair(active.digest, &missing).await;
            drop(permit);
            drop(active);
        });
    }
}

impl Shared {
    /// Copies `digest` to each of `missing` that still lacks it. A failure is logged and
    /// leaves the blob for the next read that finds it away from its owner.
    async fn repair(&self, digest: Digest, missing: &[MemberId]) {
        let Ok(view) = self.router.view() else {
            return;
        };
        let generation = view.map.generation();
        for id in missing {
            let Some(member) = view.map.member(*id) else {
                continue;
            };
            let Ok(node) = self.router.nodes.node(member, generation) else {
                continue;
            };
            match node.find_missing(&[digest]).await {
                Ok(lacking) if lacking.is_empty() => continue,
                Ok(_) => {}
                Err(e) => {
                    tracing::debug!(%digest, owner = %id, error = %e, "read repair could not ask the owner");
                    continue;
                }
            }
            match self.router.copy_blob_to(&node, digest).await {
                Ok(()) => {
                    tracing::debug!(%digest, owner = %id, "read repair copied a blob to its owner")
                }
                Err(e) => {
                    tracing::debug!(%digest, owner = %id, error = %e, "read repair could not copy a blob to its owner")
                }
            }
        }
    }
}
