//! A server's write tier for the tests: one metadata log that outlives a restart, a
//! tier directory on a `FaultFs`, and a faulty store, so a test can crash, restart and
//! look at every piece.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use bytes::Bytes;
use kbf_front::metrics::COMMANDS;
use kbf_front::{
    Cache, MemoryMetaLog, MetaLog, MetaLogError, TierStartError, TierStartup, VerifiedBlob,
};
use kbf_meta::{
    Applied, BlobAnswer, Command, Location, MetaState, ObjectId, Retention, ServerId, StagedState,
    TierId, UnreachableReason,
};
use kbf_objstore::{Capabilities, KeyPrefix};
use kbf_store::{FaultFs, Fs};
use kbf_tier::{TierOptions, block_name, seg_name};
use kbf_types::Digest;
use tokio::sync::Notify;

use crate::common::faulty::FaultyStore;
use crate::common::pseudo_random;

pub const SERVER: ServerId = ServerId::new(1);
pub const SEAL: Duration = TierOptions::SEAL_AFTER;
/// The cache's segment limit in these tests: a blob over it gets a block of its own.
pub const SEGMENT: u64 = 256 << 10;
pub const CONDITIONAL: Capabilities = Capabilities {
    conditional_put: true,
    object_lock: false,
};

pub type TierCache = Cache<Log, FaultyStore>;

/// The metadata log of one farm, kept across the cache's restarts. A test can refuse
/// the next commits of a command (answered UNAVAILABLE, not applied), as a log that
/// lost its leader would, or lose their answers (applied, then answered UNAVAILABLE),
/// as a Raft log that stopped after the entry was durable would, and then fail the
/// next state read too, as a log that cannot answer at all would.
#[derive(Clone, Debug)]
pub struct Log(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    log: MemoryMetaLog,
    refuse: Mutex<BTreeMap<&'static str, usize>>,
    wrong: Mutex<BTreeMap<&'static str, usize>>,
    lose: Mutex<BTreeMap<&'static str, usize>>,
    /// Commits whose lost answer also fails the next state read.
    blind: Mutex<BTreeMap<&'static str, usize>>,
    /// State reads still to fail, and how many failed.
    fail_reads: Mutex<usize>,
    failed_reads: Mutex<usize>,
    applied: Mutex<Vec<&'static str>>,
    holds: Mutex<Holds>,
    /// Woken when a commit parks at a hold, and when a hold ends.
    parked_changed: Notify,
    released: Notify,
}

/// The commands whose commits wait at the gate ([`Log::hold`]), and how many wait.
#[derive(Debug, Default)]
struct Holds {
    held: BTreeSet<&'static str>,
    parked: BTreeMap<&'static str, usize>,
}

/// A commit parked at a hold; leaving it, finished or dropped, takes it off the count.
struct Parked<'a> {
    inner: &'a Inner,
    label: &'static str,
}

impl Drop for Parked<'_> {
    fn drop(&mut self) {
        let mut holds = lock(&self.inner.holds);
        if let Some(n) = holds.parked.get_mut(self.label) {
            *n -= 1;
        }
    }
}

impl Log {
    pub fn new() -> Self {
        Self(Arc::new(Inner {
            log: MemoryMetaLog::new(Retention::default()),
            refuse: Mutex::new(BTreeMap::new()),
            wrong: Mutex::new(BTreeMap::new()),
            lose: Mutex::new(BTreeMap::new()),
            blind: Mutex::new(BTreeMap::new()),
            fail_reads: Mutex::new(0),
            failed_reads: Mutex::new(0),
            applied: Mutex::new(Vec::new()),
            holds: Mutex::new(Holds::default()),
            parked_changed: Notify::new(),
            released: Notify::new(),
        }))
    }

    /// The next `n` commits of `command` (a [`COMMANDS`] label) are refused.
    pub fn refuse(&self, command: &'static str, n: usize) {
        assert!(COMMANDS.contains(&command), "{command}");
        *lock(&self.0.refuse).entry(command).or_default() += n;
    }

    /// The next `n` commits of `command` are not applied and answered with another
    /// command's outcome, as a broken log would answer.
    pub fn answer_wrong(&self, command: &'static str, n: usize) {
        assert!(COMMANDS.contains(&command), "{command}");
        *lock(&self.0.wrong).entry(command).or_default() += n;
    }

    /// The next `n` commits of `command` are applied and then answered UNAVAILABLE.
    #[allow(
        dead_code,
        reason = "write_tier_flush.rs uses it; write_tier.rs does not"
    )]
    pub fn lose_answer(&self, command: &'static str, n: usize) {
        assert!(COMMANDS.contains(&command), "{command}");
        *lock(&self.0.lose).entry(command).or_default() += n;
    }

    /// The next commit of `command` is applied and answered UNAVAILABLE, and the first
    /// state read after it (through the cache's [`MetaLog::query`]) fails too: armed by
    /// the lost answer itself, so no read the cache made before the commit takes it.
    #[allow(
        dead_code,
        reason = "write_tier_flush.rs uses it; write_tier.rs does not"
    )]
    pub fn lose_answer_and_next_read(&self, command: &'static str) {
        self.lose_answer(command, 1);
        *lock(&self.0.blind).entry(command).or_default() += 1;
    }

    /// The next state read through the cache's [`MetaLog::query`] fails, as a log that
    /// cannot answer would.
    #[allow(
        dead_code,
        reason = "write_tier_flusher.rs uses it; the other tests do not"
    )]
    pub fn fail_next_read(&self) {
        *lock(&self.0.fail_reads) += 1;
    }

    /// How many of the cache's state reads the test failed.
    #[allow(
        dead_code,
        reason = "write_tier_flush.rs uses it; write_tier.rs does not"
    )]
    pub fn failed_reads(&self) -> usize {
        *lock(&self.0.failed_reads)
    }

    /// Commits `command` straight to the log, as another server would.
    pub fn commit(&self, command: Command) -> Applied {
        futures::executor::block_on(self.0.log.commit(command)).expect("memory log")
    }

    /// How many commits of `command` applied.
    pub fn applied(&self, command: &str) -> usize {
        lock(&self.0.applied)
            .iter()
            .filter(|c| **c == command)
            .count()
    }

    /// How many commits of any command applied.
    #[allow(
        dead_code,
        reason = "only the tests of the write pool's rig count them"
    )]
    pub fn applied_count(&self) -> usize {
        lock(&self.0.applied).len()
    }

    /// From now on a commit of `command` (a [`COMMANDS`] label) waits, before it is applied
    /// or refused, until [`Log::release`]: a log whose leader has not answered yet.
    #[allow(
        dead_code,
        reason = "only the tests of the write pool's rig hold a commit"
    )]
    pub fn hold(&self, command: &'static str) {
        assert!(COMMANDS.contains(&command), "{command}");
        lock(&self.0.holds).held.insert(command);
    }

    /// Ends the hold on `command`: the commits parked at it go on, and later ones do not
    /// wait.
    #[allow(
        dead_code,
        reason = "only the tests of the write pool's rig hold a commit"
    )]
    pub fn release(&self, command: &'static str) {
        lock(&self.0.holds).held.remove(command);
        self.0.released.notify_waiters();
    }

    /// Waits until `n` commits of `command` are parked at its hold: a positive event, not
    /// a poll.
    #[allow(
        dead_code,
        reason = "only the tests of the write pool's rig hold a commit"
    )]
    pub async fn wait_held(&self, command: &'static str, n: usize) {
        loop {
            let changed = self.0.parked_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if lock(&self.0.holds)
                .parked
                .get(command)
                .copied()
                .unwrap_or(0)
                >= n
            {
                return;
            }
            changed.await;
        }
    }

    /// Waits at the hold on `label`, if there is one.
    async fn wait_at_hold(&self, label: &'static str) {
        let mut parked = None;
        loop {
            let released = self.0.released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            {
                let mut holds = lock(&self.0.holds);
                if !holds.held.contains(label) {
                    return;
                }
                if parked.is_none() {
                    *holds.parked.entry(label).or_default() += 1;
                    parked = Some(Parked {
                        inner: &self.0,
                        label,
                    });
                    self.0.parked_changed.notify_waiters();
                }
            }
            released.await;
        }
    }

    /// Reads the committed state.
    pub fn state<R: Send>(&self, f: impl FnOnce(&MetaState) -> R + Send) -> R {
        futures::executor::block_on(self.0.log.query(f)).expect("memory log")
    }

    pub fn answer(&self, digest: &Digest) -> BlobAnswer {
        self.state(|s| s.blob(digest))
    }

    /// Where `digest` is and the tier it is staged on; panics unless present.
    pub fn located(&self, digest: &Digest) -> (Location, Option<TierId>) {
        match self.answer(digest) {
            BlobAnswer::Present {
                location,
                staged_on,
            } => (location, staged_on),
            other => panic!("{digest} is {other:?}"),
        }
    }

    pub fn object_of(&self, digest: &Digest) -> ObjectId {
        self.located(digest).0.object
    }

    pub fn staged_state(&self, object: ObjectId) -> Option<StagedState> {
        self.state(|s| s.staged(object).map(kbf_meta::StagedObject::state))
    }

    /// The blobs lost now and ever (`kbf_cache_blob_losses_total`'s source): both must
    /// stay at zero for anything but a real loss.
    #[allow(dead_code, reason = "the tests of the flusher use it; others do not")]
    pub fn losses(&self) -> (u64, u64) {
        self.state(|s| (s.lost_blobs(), s.blob_losses()))
    }

    pub fn unreachable(&self, object: ObjectId) -> Option<UnreachableReason> {
        self.state(|s| s.unreachable(object))
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Log {
    /// [`MetaLog::commit`] for a process that can die: `alive` is asked after the hold and
    /// before the command is applied, with nothing awaited in between, so a process killed
    /// while its proposal is parked never applies it, however the release and the kill
    /// interleave.
    #[allow(dead_code, reason = "only the chain rig kills a node")]
    pub async fn commit_while(
        &self,
        command: Command,
        alive: impl Fn() -> bool + Send + Sync,
    ) -> Result<Applied, MetaLogError> {
        self.commit_checked(command, Some(&alive)).await
    }

    async fn commit_checked(
        &self,
        command: Command,
        alive: Option<&(dyn Fn() -> bool + Send + Sync)>,
    ) -> Result<Applied, MetaLogError> {
        let label = COMMANDS[kbf_front::metrics::command_label(&command)];
        self.wait_at_hold(label).await;
        if alive.is_some_and(|alive| !alive()) {
            return Err(MetaLogError::Unavailable(format!(
                "{label} proposed by a process that died"
            )));
        }
        {
            let mut refuse = lock(&self.0.refuse);
            if let Some(n) = refuse.get_mut(label).filter(|n| **n > 0) {
                *n -= 1;
                return Err(MetaLogError::Unavailable(format!(
                    "{label} refused by the test"
                )));
            }
        }
        {
            let mut wrong = lock(&self.0.wrong);
            if let Some(n) = wrong.get_mut(label).filter(|n| **n > 0) {
                *n -= 1;
                return Ok(Applied::Collected(kbf_meta::Collected::default()));
            }
        }
        lock(&self.0.applied).push(label);
        let applied = self.0.log.commit(command).await;
        let mut lose = lock(&self.0.lose);
        if let Some(n) = lose.get_mut(label).filter(|n| **n > 0) {
            *n -= 1;
            let mut blind = lock(&self.0.blind);
            if let Some(n) = blind.get_mut(label).filter(|n| **n > 0) {
                *n -= 1;
                *lock(&self.0.fail_reads) += 1;
            }
            return Err(MetaLogError::Unavailable(format!(
                "{label} applied, its answer lost by the test"
            )));
        }
        applied
    }
}

impl MetaLog for Log {
    async fn commit(&self, command: Command) -> Result<Applied, MetaLogError> {
        self.commit_checked(command, None).await
    }

    async fn query<R, F>(&self, f: F) -> Result<R, MetaLogError>
    where
        F: FnOnce(&MetaState) -> R + Send,
        R: Send,
    {
        {
            let mut fail = lock(&self.0.fail_reads);
            if *fail > 0 {
                *fail -= 1;
                *lock(&self.0.failed_reads) += 1;
                return Err(MetaLogError::Unavailable(
                    "a state read failed by the test".to_owned(),
                ));
            }
        }
        self.0.log.query(f).await
    }
}

pub fn options(budget: u64) -> TierOptions {
    TierOptions::new(budget)
}

/// One start of the server: a cache over `log` and `store` in a new writer epoch, with
/// `fs` attached as its tier of `budget` bytes.
pub async fn start(
    fs: &FaultFs,
    log: &Log,
    store: &FaultyStore,
    budget: u64,
    abandon: Option<TierId>,
) -> Result<(TierCache, TierStartup), TierStartError> {
    let cache = Cache::open(log.clone(), store.clone(), KeyPrefix::default())
        .await
        .expect("open")
        .with_segment_limit(SEGMENT);
    cache
        .attach_tier(fs.clone(), options(budget), SERVER, abandon)
        .await
}

/// A farm's pieces: the tier's disk, the log and the store.
pub struct Farm {
    pub fs: FaultFs,
    pub log: Log,
    pub store: FaultyStore,
}

impl Farm {
    pub fn new(capabilities: Capabilities) -> Self {
        Self {
            fs: FaultFs::new(),
            log: Log::new(),
            store: FaultyStore::new(capabilities),
        }
    }

    /// The first start, with a tier of `budget` bytes.
    pub async fn start(&self, budget: u64) -> TierCache {
        start(&self.fs, &self.log, &self.store, budget, None)
            .await
            .expect("start")
            .0
    }

    /// The bytes every file in the tier directory holds.
    pub fn disk_bytes(&self) -> u64 {
        let names = self.fs.list().expect("list");
        names
            .iter()
            .map(|n| self.fs.file_len(n).expect("len"))
            .sum()
    }

    pub fn has_block(&self, object: ObjectId) -> bool {
        self.fs.contents(&block_name(object)).is_some()
    }

    pub fn has_copy(&self, object: ObjectId) -> bool {
        self.fs.contents(&seg_name(object)).is_some()
    }
}

/// A blob's bytes and digest.
#[derive(Clone, Debug)]
pub struct TestBlob {
    pub bytes: Bytes,
    pub digest: Digest,
}

impl TestBlob {
    /// `len` bytes for `seed`; every seed gives other bytes (`pseudo_random` gives
    /// seeds `2n` and `2n + 1` the same ones, so only odd seeds reach it).
    pub fn new(seed: u64, len: usize) -> Self {
        let bytes = Bytes::from(pseudo_random(len, seed * 2 + 1));
        let digest = kbf_segments::sha256(&bytes);
        Self { bytes, digest }
    }

    pub fn verified(&self) -> VerifiedBlob {
        VerifiedBlob::new(self.digest, self.bytes.clone()).expect("verified")
    }
}

/// `n` blobs of `len` bytes, seeds `from..from + n`.
pub fn blobs(from: u64, n: u64, len: usize) -> Vec<TestBlob> {
    (from..from + n)
        .map(|seed| TestBlob::new(seed, len))
        .collect()
}

pub fn verified(blobs: &[TestBlob]) -> Vec<VerifiedBlob> {
    blobs.iter().map(TestBlob::verified).collect()
}

/// A time past every grouped block's seal delay.
pub fn later() -> Instant {
    Instant::now() + SEAL
}

/// The key the cache gives `object`.
pub fn key(cache: &TierCache, object: ObjectId) -> kbf_objstore::ObjectKey {
    cache.object_key(object).expect("key")
}
