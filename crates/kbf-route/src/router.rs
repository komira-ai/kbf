//! [`Router`]: the map, the nodes and the rules that every call shares.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use kbf_map::{Map, Member, MemberState};
use kbf_storage::StorageError;
use kbf_types::Digest;
use tokio::sync::Semaphore;

use crate::closure::{DEFAULT_CLOSURE_ENTRIES, DEFAULT_CLOSURE_TTL, Verified};
use crate::flights::Flights;
use crate::hold::{Hold, HoldClock};
use crate::nodes::Nodes;
use crate::repair::{Background, Limiter, ReadRepair, RepairLimit};
use crate::source::{MapSource, MapView};

/// What [`Router::commit_with_closure`] holds in memory, across all its calls, unless
/// [`Router::with_copy_budget`] says otherwise: 256 MiB.
pub const DEFAULT_COPY_BYTES: u64 = 256 << 20;

const DEFAULT_COPY_KIB: u32 = (DEFAULT_COPY_BYTES / 1024) as u32;

/// Calls to the storage nodes of a farm, routed by the storage map.
///
/// Cheap to clone; every clone shares the map source, the nodes, the repair limit and the
/// reads in flight. What each call does, and what it does not, is in the crate
/// documentation.
#[derive(Clone)]
pub struct Router {
    pub(crate) maps: Arc<dyn MapSource>,
    pub(crate) nodes: Arc<dyn Nodes>,
    pub(crate) repair: Option<Arc<Repair>>,
    pub(crate) flights: Arc<Flights>,
    /// The bytes of blobs that [`Router::commit_with_closure`] holds in memory to copy, in
    /// KiB, shared by every clone.
    pub(crate) copying: Arc<Semaphore>,
    pub(crate) copy_kib: u32,
    /// The blobs a closure check found held, shared by every clone.
    pub(crate) verified: Arc<Verified>,
    /// How long a call that no owner could answer is held and tried again.
    pub(crate) hold: Hold,
}

/// The read-repair hook and its limit.
pub(crate) struct Repair {
    pub hook: Arc<dyn ReadRepair>,
    pub limiter: Limiter,
}

/// Who holds a digest, as a caller that talks to the nodes itself (the frontend's
/// pass-through of a ByteStream) needs to know.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    /// The generation of the map this was worked out from.
    pub generation: u64,
    /// The members to read from, in the order to try them: this generation's owners in
    /// rank order, then the members that may not hold the blob yet, then, while a change
    /// settles, the previous generation's owners. Empty when every owner is down.
    pub read: Vec<Member>,
    /// The owners of this generation that are down, and so are not in `read`. A blob that
    /// no member of `read` holds may be on one of these: the caller is not told it is
    /// missing while any of them is down.
    pub away: Vec<Member>,
    /// The member a write goes to: the first owner that is not down. `None` when every
    /// owner is down.
    pub write: Option<Member>,
    /// Every owner a write may go to, in the order [`Router::put`] tries them: this
    /// generation's write owners that are not down, primary first (so `write` is the first
    /// of them). A caller that passes a write through itself and wants to do what the
    /// router does when the primary fails goes on to the next of these.
    pub writers: Vec<Member>,
}

impl Router {
    /// A router over `maps`, calling nodes through `nodes`.
    #[must_use]
    pub fn new(maps: Arc<dyn MapSource>, nodes: Arc<dyn Nodes>) -> Self {
        Self {
            maps,
            nodes,
            repair: None,
            flights: Arc::default(),
            copying: Arc::new(Semaphore::new(DEFAULT_COPY_KIB as usize)),
            copy_kib: DEFAULT_COPY_KIB,
            verified: Arc::new(Verified::new(DEFAULT_CLOSURE_ENTRIES, DEFAULT_CLOSURE_TTL)),
            hold: Hold::none(),
        }
    }

    /// The same router, holding a call that no owner of its blob could answer (every
    /// owner `Down` in the map, or every attempt UNAVAILABLE) and trying it again, with
    /// growing pauses, until an owner answers or `bound` has passed since the call began;
    /// then the call is UNAVAILABLE, as it is without a hold. A router made by
    /// [`Router::new`] holds nothing (a bound of zero). The frontend and the daemon use
    /// [`crate::DEFAULT_HOLD`].
    ///
    /// The hold covers every call that takes a blob or an entry to its owners, and a read
    /// before its first byte (an owner that accepts the read and fails as its first frame
    /// is one that could not answer). A read that has passed bytes on and then fails goes
    /// on ([`crate::MAX_RESUMES`]) as it always did, without a hold: the caller already
    /// has part of the stream, and a bound that long is the caller's to wait out. A call
    /// that fails for another reason (no map yet, a map that keeps moving) is not held.
    /// Every try reads the map again, so an owner that is marked `Down` and then registers
    /// again is found.
    ///
    /// A read of a blob that some owners could not answer and the others lack is held as
    /// well: the blob may be on the owner that is away. A call that an owner answered, with
    /// the blob or without the entry, is not.
    ///
    /// Callers of one blob share a read ([`Router::read_blob`]), which holds as the router
    /// of the caller that started it does: a caller that joins a read begun through a
    /// router that holds nothing (read repair) gets its answer at once.
    #[must_use]
    pub fn with_hold(mut self, bound: Duration) -> Self {
        self.hold.bound = bound;
        self
    }

    /// The same router, holding by `clock` (the time, and the sleeping between tries)
    /// instead of Tokio's: for a test that runs a hold without real time.
    #[must_use]
    pub fn with_hold_clock(mut self, clock: Arc<dyn HoldClock>) -> Self {
        self.hold = self.hold.with_clock(clock);
        self
    }

    /// The same router, holding nothing.
    pub(crate) fn unheld(mut self) -> Self {
        self.hold.bound = Duration::ZERO;
        self
    }

    /// The same router, holding at most `bytes` of blobs in memory at once (counted in
    /// KiB, at least one) across every [`Router::commit_with_closure`] on it and its
    /// clones. A blob larger than that is copied alone. The default is
    /// [`DEFAULT_COPY_BYTES`].
    #[must_use]
    pub fn with_copy_budget(mut self, bytes: u64) -> Self {
        let kib = u32::try_from(bytes / 1024).unwrap_or(u32::MAX).max(1);
        self.copying = Arc::new(Semaphore::new(kib as usize));
        self.copy_kib = kib;
        self
    }

    /// The same router, telling `hook` about a blob a read found away from where it
    /// belongs, at most as often as `limit` allows.
    #[must_use]
    pub fn with_read_repair(mut self, hook: Arc<dyn ReadRepair>, limit: RepairLimit) -> Self {
        self.repair = Some(Arc::new(Repair {
            hook,
            limiter: Limiter::new(limit),
        }));
        self
    }

    /// The same router, copying a blob a read or [`Router::find_missing_anywhere`] found
    /// away from the owner that should hold it to that owner, in the background, at most
    /// as often as `limit` lets a find through ([`Router::with_read_repair`] with the
    /// router's own hook).
    ///
    /// The copy never delays the call that found the blob: it runs in a task of its own
    /// (so the call that finds a blob must run inside a Tokio runtime, or the find is
    /// not repaired), at most [`crate::MAX_REPAIRS_IN_FLIGHT`] at a time, one per digest, and a
    /// find that arrives while the copies are all busy is dropped: a later read finds the
    /// blob away from its owner again and tells the hook again. A copy is read from
    /// wherever the map has it, hashed against its digest, and written with
    /// [`kbf_storage::Storage::put_unowned`]; the owner is asked first whether it still
    /// lacks the blob.
    #[must_use]
    pub fn with_background_repair(self, limit: RepairLimit) -> Self {
        // The task reads through a router without the hook, so a repair's own read is
        // never reported as a find.
        let mut plain = self.clone().unheld();
        plain.repair = None;
        self.with_read_repair(Arc::new(Background::new(plain)), limit)
    }

    /// Who holds `digest` in the map as it is now.
    ///
    /// # Errors
    /// No map has arrived yet.
    pub fn place(&self, digest: &Digest) -> Result<Placement, StorageError> {
        let view = self.view()?;
        let writers: Vec<Member> = write_order(&view.map, digest)
            .into_iter()
            .cloned()
            .collect();
        Ok(Placement {
            generation: view.map.generation(),
            read: read_order(&view, digest).into_iter().cloned().collect(),
            away: view.map.down_owners(digest).into_iter().cloned().collect(),
            write: writers.first().cloned(),
            writers,
        })
    }

    /// A number that changes whenever what a "this blob is held" answer rests on may have
    /// changed: the map's generation, and the id and boot of every member in it. A caller
    /// that remembers an answer beside this number may trust it only while the number is
    /// the same: a new generation moves blobs between owners, and a restarted owner (a
    /// new boot) is the one whose holdings are least certain (a disk replaced or wiped
    /// while it was down, say).
    ///
    /// `None` when no map has arrived, or when any member's boot is 0 (unknown: a restart
    /// of that member could not be told), so that nothing is remembered.
    #[must_use]
    pub fn epoch(&self) -> Option<u64> {
        use std::hash::{Hash as _, Hasher as _};
        let view = self.view().ok()?;
        let members = view.map.members();
        if members.iter().any(|m| m.boot == 0) {
            return None;
        }
        // The default hasher has fixed keys: equal inputs give equal numbers.
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        view.map.generation().hash(&mut hasher);
        for member in members {
            (member.id, member.boot).hash(&mut hasher);
        }
        Some(hasher.finish())
    }

    /// How many blobs are being read for waiting callers right now: for a gauge, and for
    /// tests.
    #[must_use]
    pub fn reads_in_flight(&self) -> usize {
        self.flights.len()
    }

    /// The map now, or UNAVAILABLE before the first one.
    pub(crate) fn view(&self) -> Result<MapView, StorageError> {
        self.maps
            .current()
            .ok_or_else(|| unavailable("no storage map has arrived yet".to_owned()))
    }

    /// [`Router::attempt`], and while it answers UNAVAILABLE, again after a pause
    /// ([`Router::with_hold`]) until it answers otherwise or the bound has passed, when the
    /// last UNAVAILABLE is the answer. Each try is made against the map as it is then. The
    /// UNAVAILABLE of the routing itself (no map yet, a map that keeps moving) is not held.
    pub(crate) async fn routed<T, Fut>(
        &self,
        op: impl Fn(MapView) -> Fut,
    ) -> Result<T, StorageError>
    where
        Fut: Future<Output = Result<T, StorageError>>,
    {
        let mut waiting = self.hold.begin();
        loop {
            let done = self.attempt(&op).await?;
            if !matches!(done, Err(StorageError::Unavailable(_))) {
                return done;
            }
            if !waiting.again().await {
                return done;
            }
        }
    }

    /// Runs `op` against the map as it is now. Each time a node answers `STALE_MAP`, fetches
    /// the newer map and runs it again against that: the map can move several times
    /// during one call (every member's promotion is a new generation). It stops, with
    /// UNAVAILABLE rather than a loop, when a fetch brings no map newer than the one just
    /// used (the generation has stopped moving where this process can see it), or after
    /// [`MAX_MAP_REFETCHES`] fetches in one call. The outer error is that of the routing;
    /// the inner is what `op` answered.
    async fn attempt<T, Fut>(
        &self,
        op: &impl Fn(MapView) -> Fut,
    ) -> Result<Result<T, StorageError>, StorageError>
    where
        Fut: Future<Output = Result<T, StorageError>>,
    {
        let mut view = self.view()?;
        let mut fetches = 0;
        loop {
            let used = view.map.generation();
            match op(view).await {
                Err(StorageError::StaleMap { current }) => {
                    view = self.newer_map(used, current, &mut fetches).await?;
                }
                done => return Ok(done),
            }
        }
    }

    /// The map to run a call again against after a node holding generation `current`
    /// refused the one of generation `used`: fetched, and newer than `used`. `fetches`
    /// counts the fetches the call has made; it fails UNAVAILABLE, rather than loop, when
    /// the fetch brings no newer map or [`MAX_MAP_REFETCHES`] have been made. For a caller
    /// that dials the nodes itself and so runs the retry itself, by the same rule.
    ///
    /// # Errors
    /// UNAVAILABLE, as above.
    pub async fn newer_map(
        &self,
        used: u64,
        current: u64,
        fetches: &mut usize,
    ) -> Result<MapView, StorageError> {
        if *fetches == MAX_MAP_REFETCHES {
            return Err(unavailable(format!(
                "the storage map moved {fetches} times during one call; a node now holds \
                 generation {current}"
            )));
        }
        *fetches += 1;
        let fresh = self
            .maps
            .refetch(current)
            .await
            .map_err(|e| unavailable(e.to_string()))?;
        if fresh.map.generation() <= used {
            return Err(unavailable(format!(
                "a node holds map generation {current}, but the newest map is {}",
                fresh.map.generation()
            )));
        }
        Ok(fresh)
    }
}

/// The most map fetches one routed call makes when nodes keep answering `STALE_MAP`: room
/// for a run of promotions during one call, and a bound on a map that never settles.
pub const MAX_MAP_REFETCHES: usize = 8;

/// This generation's read order of `digest`, then each retained earlier generation's, newest
/// first.
pub(crate) fn read_order<'a>(view: &'a MapView, digest: &Digest) -> Vec<&'a Member> {
    view.map.read_order_with_history(&view.history(), digest)
}

/// The owners a write of `digest` may go to: its write owners that are not down, primary
/// first. A `Down` member keeps its slot in the map for the dwell; a caller skips it.
pub(crate) fn write_order<'a>(map: &'a Map, digest: &Digest) -> Vec<&'a Member> {
    map.write_owners(digest)
        .into_iter()
        .filter_map(|id| map.member(id))
        .filter(|m| m.state != MemberState::Down)
        .collect()
}

/// An outage of the routing itself.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct Routing(String);

pub(crate) fn unavailable(message: String) -> StorageError {
    StorageError::Unavailable(Arc::new(Routing(message)))
}
