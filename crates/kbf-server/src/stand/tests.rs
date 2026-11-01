//! A retire that stands down with requeues the state store has not taken yet.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use futures::future::AbortRegistration;
use kbf_front::{Backend, Cache, Described, Dispatch, Submission};
use kbf_sched::{DaemonInstance, Request};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{
    Changes, Commit, CommitStats, Conflict, ManualClock, Revision, ScanPage, ScanRange, Store,
    StoreError, StoreTime, Txn, Versioned,
};
use kbf_types::{ActionKey, Digest, DigestFunction, LeaseKind, Qos, Resources, WorkerId};
use tokio::sync::mpsc;

use super::*;
use crate::farm::{EpochPolicy, Outbound, Reported, StreamId};
use crate::health::Readiness;
use crate::state_store::StateStore;
use crate::test_log::capture;

/// A state store over the fake that can refuse writes, as a store with a failing disk
/// does, and counts the batches of writes it is asked to apply.
struct Refuses {
    inner: Fake,
    /// How many of the next batches of writes the store refuses (as a failure of the
    /// backend), counting down by one per refusal.
    refuse_next: AtomicUsize,
    /// Every write answers that a newer writer exists ([`StoreError::Fenced`]).
    newer_writer: AtomicBool,
    /// Batches of writes the store was asked to apply, refused or not.
    writes_tried: AtomicUsize,
}

impl Refuses {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None),
            refuse_next: AtomicUsize::new(0),
            newer_writer: AtomicBool::new(false),
            writes_tried: AtomicUsize::new(0),
        })
    }

    fn put(&self, key: &str, value: Vec<u8>) {
        let put = Txn::new().put(key, value, kbf_state::Cond::Any);
        self.inner.txn(put).expect("store").expect("put");
    }
}

impl Store for Refuses {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        self.writes_tried.fetch_add(1, Ordering::SeqCst);
        if self.newer_writer.load(Ordering::SeqCst) {
            return Err(StoreError::Fenced);
        }
        let refused = self
            .refuse_next
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
        if refused.is_ok() {
            return Err(StoreError::Failed(
                "the store refuses this write".to_owned(),
            ));
        }
        self.inner.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.inner.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.inner.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.changes(after, limit)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.inner.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.inner.now()
    }
}

/// A request to run, distinguished by `tag`.
fn request(tag: u8) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
        }
        .into(),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
        whole_worker: false,
    }
}

/// Queues `request` on `farm`.
fn submit(farm: &Farm, request: Request) -> Result<kbf_front::Ticket, Status> {
    farm.submit(Submission {
        request,
        call: None,
        described: Described::default(),
        evidence: Default::default(),
        max_wall: None,
    })
}

/// A serving frontend's parts, as a retire stands them down: a farm over a store, a drain,
/// the stop of the worker listener and the task that expires rows.
struct Rig {
    store: Arc<Refuses>,
    backend: Backend,
    farm: Arc<Farm>,
    drain: Arc<Drain>,
    /// The drain's readiness, which a retire marks handed over as it begins.
    readiness: Arc<Readiness>,
    worker: Stop,
    sweeper: AbortHandle,
    /// Kept so the sweeper is not aborted before it is told to be.
    _sweeping: AbortRegistration,
}

impl Rig {
    fn new() -> Self {
        let store = Refuses::new();
        // A store opened for a writer holds a generation.
        store.put(kbf_state::GENERATION, 7_u64.to_be_bytes().to_vec());
        let backend = Backend::from(Arc::new(Cache::memory()));
        let farm = Farm::new(
            backend.clone(),
            kbf_sched::UNSERVABLE_WAIT,
            kbf_sched::FINISHED_RETENTION,
        )
        .with_state(Some(StateStore {
            store: store.clone(),
            epoch: EpochPolicy::Keep,
            members: Default::default(),
            history: None,
        }))
        .expect("a farm over the store");
        let (closer, _closing) = kbf_front::closing();
        let readiness = Arc::new(Readiness::default());
        let (drain, _goaway) = Drain::new(closer, Arc::clone(&readiness));
        let (sweeper, _sweeping) = AbortHandle::new_pair();
        Self {
            store,
            backend,
            farm: Arc::new(farm),
            drain: Arc::new(drain),
            readiness,
            worker: Stop::new(),
            sweeper,
            _sweeping,
        }
    }

    fn stand(&self, wait: Duration) -> Stand {
        Stand {
            drain: Arc::clone(&self.drain),
            farm: Arc::clone(&self.farm),
            worker: self.worker.clone(),
            sweeper: self.sweeper.clone(),
            wait,
        }
    }
}

fn node() -> WorkerId {
    WorkerId::new("node-a")
}

const LONG: Duration = Duration::from_secs(600);

fn register(rig: &Rig, instance: &str, predecessor: &str, outbound: Outbound) -> StreamId {
    let caps = kbf_caps::NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    rig.farm.register(
        &node(),
        DaemonInstance::new(instance),
        DaemonInstance::new(predecessor),
        Reported::serving(Resources::new(4_000, 8 << 30), caps),
        outbound,
        Default::default(),
    )
}

fn keys(rig: &Rig, prefix: &str) -> Vec<String> {
    let rows = rig.store.scan_prefix(prefix).expect("scan");
    rows.into_iter().map(|(key, _)| key).collect()
}

/// A farm that holds one operation, leased to a node, whose lease the node's replacement
/// has just given up while the state store refused every write: the requeue is unsettled,
/// and the store still holds the stale lease and no queue row. Returns the operation's
/// name in the store.
async fn with_an_unsettled_requeue(rig: &Rig) -> String {
    let (outbound, inbox) = mpsc::unbounded_channel();
    register(rig, "daemon", "", outbound);
    let _ticket = submit(&rig.farm, request(1)).expect("queued");
    rig.farm.tick().await;
    assert_eq!(keys(rig, "lease/").len(), 1, "the operation is leased");
    let operation = keys(rig, "op/").remove(0).replace("op/", "");

    // The daemon's process is proven stopped and its replacement registers, the frontend
    // becoming the node's owner and writing its session row; the store refuses what follows.
    drop(inbox);
    let (outbound, _kept) = mpsc::unbounded_channel();
    let stream = register(rig, "daemon-2", "daemon", outbound);
    rig.store.refuse_next.store(usize::MAX, Ordering::SeqCst);
    // The new process lists no lease: the one the old process held is given up. Its
    // heartbeat is acknowledged, for the frontend owns the node and wrote its session row.
    assert!(rig.farm.heartbeat(&node(), stream, 1, Vec::new()).await);
    rig.store.refuse_next.store(0, Ordering::SeqCst);
    assert_eq!(
        keys(rig, "lease/").len(),
        1,
        "the stale lease is still stored"
    );
    assert!(keys(rig, "queue/").is_empty(), "nothing requeued is stored");
    operation
}

/// Asserts that a retire stands down only after the requeues its state store refused are
/// written. A store that refuses the write three times and then takes it has the
/// operation's queue row (and loss count) when the stand-down returns, and no lease row; a
/// farm over it recovers the operation queued, not leased.
#[tokio::test]
async fn a_requeue_the_store_refused_is_written_before_the_stand_down_returns() {
    let rig = Rig::new();
    let operation = with_an_unsettled_requeue(&rig).await;
    rig.store.refuse_next.store(3, Ordering::SeqCst);

    rig.stand(Duration::from_secs(30)).release().await;

    assert_eq!(keys(&rig, "queue/"), [format!("queue/{operation}")]);
    assert_eq!(keys(&rig, "loss/"), [format!("loss/{operation}")]);
    assert!(keys(&rig, "lease/").is_empty());
    assert_eq!(rig.store.refuse_next.load(Ordering::SeqCst), 0);

    let successor = crate::farm::Farm::new(
        rig.backend.clone(),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
    .with_state(Some(crate::state_store::StateStore {
        store: rig.store.clone(),
        epoch: crate::farm::EpochPolicy::Keep,
        members: Default::default(),
        history: None,
    }))
    .expect("a farm over the store");
    assert_eq!(successor.queued_operations(), 1, "recovered queued");
}

/// Asserts that a store that refuses past the deadline does not hold a retire up for ever
/// and does not pass in silence: the stand-down returns all the same, the write was tried
/// more than once, and the log names the operation.
#[tokio::test]
async fn a_requeue_the_store_refuses_past_the_deadline_is_logged_and_the_stand_down_returns() {
    let rig = Rig::new();
    let operation = with_an_unsettled_requeue(&rig).await;
    rig.store.refuse_next.store(usize::MAX, Ordering::SeqCst);
    let lines = capture();
    let before = rig.store.writes_tried.load(Ordering::SeqCst);

    rig.stand(Duration::from_millis(100)).release().await;

    assert!(rig.store.writes_tried.load(Ordering::SeqCst) >= before + 2);
    assert!(keys(&rig, "queue/").is_empty());
    let log = lines.text();
    assert!(log.contains("ERROR"), "{log}");
    assert!(log.contains(&operation), "{log}");
}

/// Asserts that a retire stops writing once a newer writer exists. A store that says so
/// (Fenced) on the first attempt is tried exactly once and the retire, whose deadline is
/// ten minutes, returns at once; and a farm that already learned it from an earlier write
/// tries none. Either way the log names the operation.
#[tokio::test]
async fn a_newer_writer_stops_the_flush_at_once() {
    let rig = Rig::new();
    let operation = with_an_unsettled_requeue(&rig).await;
    assert!(!rig.farm.superseded(), "not fenced yet");
    rig.store.newer_writer.store(true, Ordering::SeqCst);
    let lines = capture();
    let before = rig.store.writes_tried.load(Ordering::SeqCst);

    let run = tokio::time::timeout(Duration::from_secs(20), rig.stand(LONG).release()).await;

    run.expect("not waiting for the deadline");
    assert!(rig.farm.superseded(), "fenced by the newer writer");
    assert_eq!(rig.store.writes_tried.load(Ordering::SeqCst), before + 1);
    assert!(lines.text().contains(&operation), "{}", lines.text());

    // Already known: the next stand down does not try at all.
    let rig = Rig::new();
    let _operation = with_an_unsettled_requeue(&rig).await;
    rig.store.newer_writer.store(true, Ordering::SeqCst);
    rig.farm.tick().await;
    let before = rig.store.writes_tried.load(Ordering::SeqCst);
    let run = tokio::time::timeout(Duration::from_secs(20), rig.stand(LONG).release()).await;
    run.expect("not waiting for the deadline");
    assert_eq!(rig.store.writes_tried.load(Ordering::SeqCst), before);
}

/// Asserts that a flush on a farm that has nothing to write does not wait out the
/// deadline: a retire with nothing unsettled returns at once.
#[tokio::test]
async fn a_retire_with_nothing_unsettled_does_not_wait() {
    let rig = Rig::new();
    let _ticket = submit(&rig.farm, request(1)).expect("queued");
    let run = tokio::time::timeout(Duration::from_secs(20), rig.stand(LONG).release()).await;
    run.expect("no wait");
}

/// Asserts that a retire fences the farm only once no call is between its request and its
/// queueing, or once the wait is up: the farm takes work while one is in flight and refuses
/// it after, and a wait that is up fences a farm that is still busy.
#[tokio::test]
async fn a_retire_waits_for_the_farm_to_be_quiet_for_a_bounded_time() {
    let rig = Rig::new();
    let busy = rig.backend.metrics().execute_presubmit_in_flight.raise();
    let stand = rig.stand(LONG);
    let during = async {
        while !rig.readiness.is_handed_over() {
            tokio::task::yield_now().await;
        }
        assert!(
            submit(&rig.farm, request(1)).is_ok(),
            "fenced while a call was in flight"
        );
        drop(busy);
    };
    tokio::join!(stand.release(), during);
    assert!(submit(&rig.farm, request(2)).is_err());

    let rig = Rig::new();
    let _busy = rig.backend.metrics().execute_presubmit_in_flight.raise();
    rig.stand(Duration::ZERO).release().await;
    assert!(submit(&rig.farm, request(3)).is_err());
}
