//! The closure check of `GetActionResult`: an action-cache hit is served only if every
//! blob of its closure is still held by at least one of the blob's owners.
//!
//! The owner of the entry reports the entry's closure with the hit
//! ([`kbf_storage::ActionEntry::closure`]). The router groups those digests by primary
//! owner and sends each primary one FindMissing (in batches of at most
//! [`MAX_FIND_MISSING_DIGESTS`]), all primaries at once. What a primary lacks goes to the
//! replica in a second round, and so on down the owners. A blob is gone only when every
//! owner said it lacks it; then the entry is a miss and the action runs again.
//!
//! An owner that cannot answer leaves its blobs undetermined, which is a miss too (a miss
//! is always safe: the action runs again) and is never remembered.
//!
//! What was verified is remembered per blob and owner ([`Verified`]), shared by every
//! action: toolchain blobs repeat across actions, so most of a closure is usually cached.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::future::join_all;
use kbf_map::{Member, MemberId};
use kbf_storage::StorageError;
use kbf_storage::service::MAX_FIND_MISSING_DIGESTS;
use kbf_types::Digest;
use tokio::time::Instant;

use crate::router::{Router, write_order};
use crate::source::MapView;

/// How many (blob, owner) pairs [`Router`] remembers as verified unless
/// [`Router::with_closure_cache`] says otherwise: about 100 MB.
pub const DEFAULT_CLOSURE_ENTRIES: usize = 2 << 20;

/// How long a blob stays verified unless [`Router::with_closure_cache`] says otherwise.
/// It bounds how long a blob that went away (cold GC, eviction, a crash the map has not
/// noticed) can still be served.
pub const DEFAULT_CLOSURE_TTL: Duration = Duration::from_secs(45);

/// What the closure check has done, since the router was made (shared by its clones).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClosureStats {
    /// Checks that served the entry.
    pub hit: u64,
    /// Checks that found a blob gone (or could not tell), so the entry was a miss.
    pub miss: u64,
    /// Entries served without a check: the node did not report a closure, or it was
    /// empty.
    pub unchecked: u64,
    /// Blobs found in the cache of verified blobs.
    pub cache_hits: u64,
    /// Blobs looked up in it.
    pub cache_lookups: u64,
    /// FindMissing calls the checks sent.
    pub rpcs: u64,
}

impl ClosureStats {
    /// The share of looked-up blobs the cache answered, 0.0 to 1.0 (0.0 before any).
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn cache_hit_ratio(&self) -> f64 {
        if self.cache_lookups == 0 {
            0.0
        } else {
            self.cache_hits as f64 / self.cache_lookups as f64
        }
    }

    /// Checks that ran (served or missed).
    #[must_use]
    pub fn checks(&self) -> u64 {
        self.hit + self.miss
    }

    /// FindMissing calls per check that ran (0.0 before any).
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn rpcs_per_check(&self) -> f64 {
        match self.checks() {
            0 => 0.0,
            n => self.rpcs as f64 / n as f64,
        }
    }
}

#[derive(Default)]
struct Counters {
    hit: AtomicU64,
    miss: AtomicU64,
    unchecked: AtomicU64,
    cache_hits: AtomicU64,
    cache_lookups: AtomicU64,
    rpcs: AtomicU64,
}

/// One owner's verified blobs, and the map generation they were verified under.
struct Owned {
    generation: u64,
    /// digest -> (when verified, its place in `Inner::recency`)
    blobs: BTreeMap<Digest, (Instant, u64)>,
}

#[derive(Default)]
struct Inner {
    members: BTreeMap<MemberId, Owned>,
    /// Least recently used first: sequence number -> entry.
    recency: BTreeMap<u64, (MemberId, Digest)>,
    next: u64,
}

impl Inner {
    /// The entries of `member` for `generation`: those of an older generation are dropped
    /// first. `None` when the caller's generation is older than what is remembered (the
    /// caller works from an old map: it neither reads nor adds).
    fn owned(&mut self, member: MemberId, generation: u64) -> Option<&mut Owned> {
        let current = self.members.get(&member).map(|o| o.generation);
        match current {
            Some(g) if g > generation => return None,
            Some(g) if g < generation => {
                // TODO(storage-ram-ack PR1): a member's boot (map.proto Member.boot) joins
                // the generation here, so a restart inside the lease drops its entries too.
                if let Some(old) = self.members.remove(&member) {
                    for (_, seq) in old.blobs.values() {
                        self.recency.remove(seq);
                    }
                }
            }
            _ => {}
        }
        Some(self.members.entry(member).or_insert_with(|| Owned {
            generation,
            blobs: BTreeMap::new(),
        }))
    }
}

/// The blobs verified present at an owner, bounded by entries (least recently used out
/// first) and by a time to live.
pub(crate) struct Verified {
    inner: Mutex<Inner>,
    capacity: usize,
    ttl: Duration,
    counters: Counters,
}

impl Verified {
    pub(crate) fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            capacity,
            ttl,
            counters: Counters::default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // Every change leaves the two maps agreeing before it can panic.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether `digest` was verified at `member` under `generation` within the time to
    /// live. A hit counts as a use.
    fn fresh(&self, member: MemberId, generation: u64, digest: &Digest, now: Instant) -> bool {
        let mut inner = self.lock();
        let Some(owned) = inner.owned(member, generation) else {
            return false;
        };
        let Some(&(at, seq)) = owned.blobs.get(digest) else {
            return false;
        };
        if now.saturating_duration_since(at) >= self.ttl {
            owned.blobs.remove(digest);
            inner.recency.remove(&seq);
            return false;
        }
        inner.touch(member, *digest, seq);
        true
    }

    /// Remembers that `digests` were verified at `member` under `generation` at `now`.
    fn record(&self, member: MemberId, generation: u64, digests: &[Digest], now: Instant) {
        if self.capacity == 0 {
            return;
        }
        let mut inner = self.lock();
        if inner.owned(member, generation).is_none() {
            return;
        }
        for digest in digests {
            let seq = inner.next;
            inner.next += 1;
            let Some(owned) = inner.members.get_mut(&member) else {
                return;
            };
            if let Some((_, old)) = owned.blobs.insert(*digest, (now, seq)) {
                inner.recency.remove(&old);
            }
            inner.recency.insert(seq, (member, *digest));
        }
        while inner.recency.len() > self.capacity {
            let Some((_, (member, digest))) = inner.recency.pop_first() else {
                break;
            };
            if let Some(owned) = inner.members.get_mut(&member) {
                owned.blobs.remove(&digest);
            }
        }
    }
}

impl Inner {
    /// Marks the entry `(member, digest)` as just used.
    fn touch(&mut self, member: MemberId, digest: Digest, old: u64) {
        let seq = self.next;
        self.next += 1;
        self.recency.remove(&old);
        self.recency.insert(seq, (member, digest));
        if let Some(entry) = self
            .members
            .get_mut(&member)
            .and_then(|o| o.blobs.get_mut(&digest))
        {
            entry.1 = seq;
        }
    }
}

impl Router {
    /// The same router, remembering at most `entries` (blob, owner) pairs as verified, each
    /// for at most `ttl`. Zero entries remembers nothing: every check asks the owners.
    /// The defaults are [`DEFAULT_CLOSURE_ENTRIES`] and [`DEFAULT_CLOSURE_TTL`].
    #[must_use]
    pub fn with_closure_cache(mut self, entries: usize, ttl: Duration) -> Self {
        self.verified = Arc::new(Verified::new(entries, ttl));
        self
    }

    /// What the closure checks have done so far.
    #[must_use]
    pub fn closure_stats(&self) -> ClosureStats {
        let c = &self.verified.counters;
        ClosureStats {
            hit: c.hit.load(Ordering::Relaxed),
            miss: c.miss.load(Ordering::Relaxed),
            unchecked: c.unchecked.load(Ordering::Relaxed),
            cache_hits: c.cache_hits.load(Ordering::Relaxed),
            cache_lookups: c.cache_lookups.load(Ordering::Relaxed),
            rpcs: c.rpcs.load(Ordering::Relaxed),
        }
    }

    /// Whether an entry that reports `closure` may be served: `None` (not reported) and
    /// an empty closure always may. Counts the check.
    ///
    /// # Errors
    /// `STALE_MAP` from a node, for [`Router::routed`] to retry.
    pub(crate) async fn closure_served(
        &self,
        view: &MapView,
        closure: Option<&[Digest]>,
    ) -> Result<bool, StorageError> {
        let counters = &self.verified.counters;
        let Some(closure) = closure.filter(|c| !c.is_empty()) else {
            counters.unchecked.fetch_add(1, Ordering::Relaxed);
            return Ok(true);
        };
        let held = self.closure_held(view, closure).await?;
        let counter = if held { &counters.hit } else { &counters.miss };
        counter.fetch_add(1, Ordering::Relaxed);
        Ok(held)
    }

    /// Whether every blob of `closure` is held by one of its owners in `view`.
    async fn closure_held(&self, view: &MapView, closure: &[Digest]) -> Result<bool, StorageError> {
        let generation = view.map.generation();
        let verified = &self.verified;
        let now = Instant::now();
        let mut pending: BTreeSet<Digest> = BTreeSet::new();
        let mut lookups = 0_u64;
        let mut cached = 0_u64;
        for digest in closure.iter().collect::<BTreeSet<_>>() {
            lookups += 1;
            let owners = write_order(&view.map, digest);
            if owners
                .iter()
                .any(|owner| verified.fresh(owner.id, generation, digest, now))
            {
                cached += 1;
            } else {
                pending.insert(*digest);
            }
        }
        verified
            .counters
            .cache_lookups
            .fetch_add(lookups, Ordering::Relaxed);
        verified
            .counters
            .cache_hits
            .fetch_add(cached, Ordering::Relaxed);

        // Digests an owner could not answer for: undetermined, so not gone.
        let mut undetermined: BTreeSet<Digest> = BTreeSet::new();
        // Each round asks the owner of every digest still pending at the next rank.
        for rank in 0.. {
            if pending.is_empty() {
                break;
            }
            let mut by_owner: BTreeMap<MemberId, (&Member, Vec<Digest>)> = BTreeMap::new();
            let mut gone = false;
            for digest in &pending {
                let owners = write_order(&view.map, digest);
                match owners.get(rank) {
                    Some(owner) => by_owner
                        .entry(owner.id)
                        .or_insert_with(|| (owner, Vec::new()))
                        .1
                        .push(*digest),
                    // Every owner was asked. A blob an owner could not answer for might be
                    // held there: undetermined; otherwise it is gone.
                    None => gone |= !undetermined.contains(digest),
                }
            }
            if gone {
                return Ok(false);
            }
            if by_owner.is_empty() {
                break;
            }
            let answers = join_all(by_owner.values().map(|(owner, asked)| async move {
                let node = self.nodes.node(owner, generation)?;
                let mut lacking = Vec::new();
                // A node bounds the digests of one call, so a long list is several.
                for chunk in asked.chunks(MAX_FIND_MISSING_DIGESTS) {
                    verified.counters.rpcs.fetch_add(1, Ordering::Relaxed);
                    lacking.extend(node.find_missing(chunk).await?);
                }
                Ok::<_, StorageError>(lacking)
            }))
            .await;
            let now = Instant::now();
            for ((owner, asked), answer) in by_owner.values().zip(answers) {
                match answer {
                    Ok(lacking) => {
                        let lacking: BTreeSet<Digest> = lacking.into_iter().collect();
                        let present: Vec<Digest> = asked
                            .iter()
                            .filter(|d| !lacking.contains(d))
                            .copied()
                            .collect();
                        verified.record(owner.id, generation, &present, now);
                        for digest in &present {
                            pending.remove(digest);
                        }
                    }
                    Err(stale @ StorageError::StaleMap { .. }) => return Err(stale),
                    Err(_) => undetermined.extend(asked.iter().copied()),
                }
            }
        }
        // What is still pending ran out of owners: gone, or undetermined; either way the
        // entry is not served.
        Ok(pending.is_empty())
    }
}
