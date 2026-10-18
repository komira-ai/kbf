//! Submitting a request is safe to repeat: a store commit whose outcome the farm never
//! learned (the connection died after COMMIT) leaves the operation written, and the
//! client's retry must find it, not queue the action a second time. A request for an
//! action another farm already queued over the same store attaches to that operation.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kbf_front::{Cache, Described};
use kbf_sched::Request;
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{
    Changes, Commit, CommitStats, Conflict, ManualClock, Revision, ScanPage, ScanRange, Store,
    StoreError, StoreTime, Txn, Versioned,
};
use kbf_types::{ActionKey, Digest, DigestFunction, LeaseKind, OperationId, Qos, Resources};

use super::*;
use crate::state_store::StateStore;

/// A store that applies the next transaction naming an `op/` row and then reports
/// failure, as a connection lost after COMMIT does: the write is in, and nobody knows.
struct LostAck {
    inner: Fake,
    armed: AtomicBool,
}

impl LostAck {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None),
            armed: AtomicBool::new(false),
        })
    }

    fn lose_next_ack(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }
}

impl Store for LostAck {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        let submits = batch.iter().flat_map(Txn::ops).any(|op| match op {
            kbf_state::Op::Put { key, .. } => key.starts_with("op/"),
            _ => false,
        });
        let applied = self.inner.apply_batch(batch)?;
        if submits && self.armed.swap(false, Ordering::SeqCst) {
            return Err(StoreError::Failed("the connection closed".to_owned()));
        }
        Ok(applied)
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

fn request(tag: u8) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
        },
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
    }
}

fn farm_over(store: &Arc<LostAck>) -> Farm {
    Farm::new(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
    .with_state(Some(StateStore {
        store: store.clone(),
        epoch: EpochPolicy::Keep,
        members: Default::default(),
        history: None,
    }))
    .expect("a farm over the store")
}

fn submit(farm: &Farm, tag: u8) -> Result<Ticket, Status> {
    farm.submit(Submission {
        request: request(tag),
        call: None,
        described: Described::default(),
    })
}

fn rows(store: &LostAck, prefix: &str) -> usize {
    store.scan_prefix(prefix).expect("scan").len()
}

fn unfinished(farm: &Farm) -> Vec<OperationId> {
    farm.lock().sched.unfinished().collect()
}

/// Catches: a retry after a commit whose outcome was unknown that is refused for good
/// (the op row it wrote is in the way of its own retry) or that queues the action a
/// second time (two op rows, two runs). After the lost acknowledgement and one retry
/// there is exactly one op row and one queue row, one operation in the scheduler, and a
/// restart over the store recovers that one operation.
#[test]
fn a_retry_after_a_commit_of_unknown_outcome_queues_the_action_once() {
    let store = LostAck::new();
    let farm = farm_over(&store);
    store.lose_next_ack();

    let first = submit(&farm, 1);
    assert_eq!(first.map(|_| ()).unwrap_err().code(), Code::Unavailable);
    let retried = submit(&farm, 1);
    assert!(
        retried.is_ok(),
        "the retry was refused: {:?}",
        retried.err()
    );

    assert_eq!(rows(&store, "op/"), 1, "the action was written twice");
    assert_eq!(rows(&store, "queue/"), 1);
    assert_eq!(unfinished(&farm).len(), 1);
    drop(farm);
    let restarted = farm_over(&store);
    assert_eq!(
        unfinished(&restarted).len(),
        1,
        "the restart found {:?}",
        unfinished(&restarted)
    );
}

/// Catches: a retry that carries on under a different id when a request for another
/// action came between, so the unknown-outcome row of the first stays queued with
/// nobody waiting for it (a second, unwanted run) or is mistaken for the other's.
#[test]
fn a_request_between_the_lost_acknowledgement_and_its_retry_keeps_one_row_each() {
    let store = LostAck::new();
    let farm = farm_over(&store);
    store.lose_next_ack();

    assert!(submit(&farm, 1).is_err());
    assert!(submit(&farm, 2).is_ok(), "an unrelated action was refused");
    assert!(submit(&farm, 1).is_ok(), "the retry was refused");

    assert_eq!(rows(&store, "op/"), 2);
    assert_eq!(rows(&store, "queue/"), 2);
    assert_eq!(unfinished(&farm).len(), 2);
}

/// Catches: a second farm over the same store (a successor that read the store before
/// the first queued the action, or a retry that reaches the other child) that queues
/// the action again under its own id, or is refused. The second attaches to the
/// operation the store holds: one op row, one operation in each scheduler, the same id.
#[test]
fn a_second_farm_attaches_to_the_operation_the_store_holds() {
    let store = LostAck::new();
    let first = farm_over(&store);
    let second = farm_over(&store);

    assert!(submit(&first, 1).is_ok());
    let attached = submit(&second, 1);
    assert!(
        attached.is_ok(),
        "the second submit was refused: {:?}",
        attached.err()
    );

    assert_eq!(rows(&store, "op/"), 1, "the action was queued twice");
    assert_eq!(rows(&store, "queue/"), 1);
    assert_eq!(unfinished(&first), unfinished(&second));
    assert_eq!(unfinished(&second).len(), 1);
    let operation = unfinished(&second)[0];
    let waiters = second.lock().sched.waiters(operation).map_or(0, <[_]>::len);
    assert!(waiters >= 1, "the second caller waits on nothing");
}
