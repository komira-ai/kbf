//! Submitting a request is safe to repeat: a store commit whose outcome the farm never
//! learned (the connection died after COMMIT) leaves the operation written, and the
//! client's retry must find it, not queue the action a second time. A request for an
//! action another farm already queued over the same store attaches to that operation.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kbf_front::{Cache, Described, Dispatch, Submission, Ticket};
use kbf_sched::Request;
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{
    Changes, Commit, CommitStats, Conflict, ManualClock, Revision, ScanPage, ScanRange, Store,
    StoreError, StoreTime, Txn, Versioned,
};
use kbf_types::{
    ActionKey, Digest, DigestFunction, LeaseKind, OperationId, Qos, Resources, WorkKey,
};

use tonic::Code;

use super::*;
use crate::farm::Farm;
use crate::state_store::StateStore;

/// A store that applies the next transaction naming an `op/` row and then reports
/// failure, as a connection lost after COMMIT does: the write is in, and nobody knows.
struct LostAck {
    inner: Fake,
    armed: AtomicBool,
    down: AtomicBool,
    fenced: AtomicBool,
    /// One applied just before each transaction that names an `op/` row, oldest first: a
    /// write landing between the farm's look at the store and its own write.
    before_submit: std::sync::Mutex<std::collections::VecDeque<Txn>>,
}

impl LostAck {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None),
            armed: AtomicBool::new(false),
            down: AtomicBool::new(false),
            fenced: AtomicBool::new(false),
            before_submit: std::sync::Mutex::new(std::collections::VecDeque::new()),
        })
    }

    fn lose_next_ack(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }
}

impl Store for LostAck {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        if self.fenced.load(Ordering::SeqCst) {
            return Err(StoreError::Fenced);
        }
        if self.down.load(Ordering::SeqCst) {
            return Err(StoreError::Failed("the store is down".to_owned()));
        }
        let submits = batch.iter().flat_map(Txn::ops).any(|op| match op {
            kbf_state::Op::Put { key, .. } => key.starts_with("op/"),
            _ => false,
        });
        if submits && let Some(landing) = self.before_submit.lock().expect("lock").pop_front() {
            self.inner.txn(landing)?.expect("the landing write");
        }
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

fn farm_over(store: &Arc<LostAck>) -> Farm {
    Farm::new_legacy(
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
        evidence: Default::default(),
        max_wall: None,
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

fn durable_over(store: &Arc<LostAck>) -> Durable {
    let store: Arc<dyn Store> = store.clone();
    Durable::open(store, EpochPolicy::Keep, 100)
        .expect("open")
        .0
}

fn subject() -> Subject {
    Subject::of(Described::default()).0
}

fn refuse(durable: &Durable, operation: u64) {
    let refusal = kbf_types::Refusal {
        operation: OperationId(operation),
        waiters: Vec::new(),
        reason: "no worker".to_owned(),
    };
    let ending = Ending {
        finished: Finished::Failed(kbf_proto::google::rpc::Status {
            code: 3,
            message: "ended".to_owned(),
            details: Vec::new(),
        }),
        retention: Duration::from_secs(1),
    };
    durable
        .persist(&[Effect::Refuse(refusal)], Some(&ending))
        .expect("refuse");
}

fn pin_row(store: &LostAck, key: &WorkKey) -> Option<Vec<u8>> {
    let prefix = "inflight/";
    let rows = store.scan_prefix(prefix).expect("scan");
    let named = key.dedup.clone();
    rows.into_iter()
        .find(|(row, _)| row.ends_with(&named))
        .map(|(_, row)| row.value)
}

/// Catches: an in-flight row left behind by the finish (the next request for the action
/// would join an operation that is gone, and never run), and a finish that takes the row
/// of a later operation of the same action. Also that the row is the id of the
/// operation it pins.
#[test]
fn the_finish_removes_the_pin_of_its_own_operation_only() {
    let store = LostAck::new();
    let durable = durable_over(&store);
    let key = request(1).key;
    assert!(matches!(
        durable.submit(OperationId(0), &request(1), &subject()),
        Ok(Submitted::Queued(OperationId(0)))
    ));
    assert_eq!(pin_row(&store, &key), Some(0_u64.to_be_bytes().to_vec()));
    refuse(&durable, 0);
    assert_eq!(rows(&store, "inflight/"), 0, "the finish left the pin");

    // A pin that names another operation than the one finishing stays.
    assert!(matches!(
        durable.submit(OperationId(1), &request(1), &subject()),
        Ok(Submitted::Queued(OperationId(1)))
    ));
    let unpinned = durable.unpin(Txn::new(), OperationId(1)).expect("unpin");
    assert_eq!(unpinned.ops().len(), 1, "the owner's finish keeps its pin");
    let other = Txn::new().put(
        format!("inflight/6d61696e/{}", key.dedup),
        9_u64.to_be_bytes(),
        Cond::Any,
    );
    store.txn(other).expect("store").expect("put");
    let unpinned = durable.unpin(Txn::new(), OperationId(1)).expect("unpin");
    assert!(
        unpinned.ops().is_empty(),
        "operation 1 took operation 9's pin"
    );
    refuse(&durable, 1);
    assert_eq!(rows(&store, "inflight/"), 1, "the other's pin went");
}

/// Catches: a pin whose operation is gone (a writer that died between the two, an
/// older build's finish) that keeps the action from ever being queued again.
#[test]
fn a_pin_of_a_finished_operation_is_replaced() {
    let store = LostAck::new();
    let durable = durable_over(&store);
    let stale = Txn::new().put(
        format!("inflight/{}/{}", "6d61696e", request(1).key.dedup),
        7_u64.to_be_bytes(),
        Cond::Absent,
    );
    store.txn(stale).expect("store").expect("put");

    let submitted = durable.submit(OperationId(0), &request(1), &subject());

    assert!(
        matches!(submitted, Ok(Submitted::Queued(OperationId(0)))),
        "{submitted:?}"
    );
    assert_eq!(
        pin_row(&store, &request(1).key),
        Some(0_u64.to_be_bytes().to_vec())
    );
    assert_eq!(rows(&store, "op/"), 1);
}

/// Catches: a request that may not join a twin (networked, or told not to cache) being
/// pinned, so a second caller joins a run that is not interchangeable with its own.
#[test]
fn a_request_that_cannot_join_has_no_pin() {
    let store = LostAck::new();
    let durable = durable_over(&store);
    let mut networked = request(1);
    networked.hermetic = false;
    for id in 0..2 {
        let submitted = durable.submit(OperationId(id), &networked, &subject());
        assert!(
            matches!(submitted, Ok(Submitted::Queued(_))),
            "{submitted:?}"
        );
    }
    assert_eq!(rows(&store, "inflight/"), 0);
    assert_eq!(rows(&store, "op/"), 2);
}

/// Asserts that a farm whose next id has a row in the way (an earlier writer's, which it
/// never saw) is not refused for good and does not write over the other's operation: it
/// takes the next id of its block, and its scheduler hands out ids past it.
#[test]
fn a_farm_whose_next_id_has_a_row_takes_the_next_id() {
    let store = LostAck::new();
    let farm = farm_over(&store);
    let other = durable_over(&store);
    assert!(
        other
            .submit(OperationId(0), &request(2), &subject())
            .is_ok()
    );

    assert!(submit(&farm, 1).is_ok(), "refused behind the counter");
    assert!(submit(&farm, 3).is_ok());

    assert_eq!(rows(&store, "op/"), 3);
    let ids: Vec<_> = unfinished(&farm).iter().map(|op| op.0).collect();
    assert_eq!(ids, [1, 2], "the farm's operations");
}

/// Asserts that a request that joins an operation another writer queued and leased on a
/// node this farm does not own does not take the operation up, neither as queued (the action
/// would run twice) nor with the lease (the farm would give up a lease it does not own, and
/// delete the owner's lease row): the farm holds nothing of it, the store's rows stand, and
/// the caller follows the operation through the store.
#[test]
fn a_joined_operation_leased_on_a_node_the_farm_does_not_own_is_followed_not_taken_up() {
    let store = LostAck::new();
    let farm = farm_over(&store);
    let other = durable_over(&store);
    assert!(
        other
            .submit(OperationId(0), &request(1), &subject())
            .is_ok()
    );
    let start = kbf_types::StartLease {
        worker: kbf_types::WorkerId::new("node-a"),
        lease: kbf_types::LeaseId::new(5, 0),
        operation: OperationId(0),
        key: request(1).key,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        fence: kbf_types::FencePolicy::RunOn,
        device: None,
    };
    other.persist(&[Effect::Start(start)], None).expect("grant");
    // Another action's operation holds a lease too; it is not this one's.
    assert!(
        other
            .submit(OperationId(1), &request(2), &subject())
            .is_ok()
    );
    let beside = kbf_types::StartLease {
        worker: kbf_types::WorkerId::new("node-b"),
        lease: kbf_types::LeaseId::new(5, 1),
        operation: OperationId(1),
        key: request(2).key,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        fence: kbf_types::FencePolicy::RunOn,
        device: None,
    };
    other
        .persist(&[Effect::Start(beside)], None)
        .expect("grant");

    assert!(submit(&farm, 1).is_ok());

    let state = farm.lock();
    assert!(
        state
            .side
            .machine
            .sent(kbf_types::LeaseId::new(5, 0))
            .is_none()
    );
    assert!(
        state
            .side
            .machine
            .sent(kbf_types::LeaseId::new(5, 1))
            .is_none()
    );
    let held = state.sched.state(OperationId(0));
    assert!(held.is_none(), "the farm holds it");
    assert_eq!(state.intake.following(), 1);
    drop(state);
    assert_eq!((rows(&store, "lease/"), rows(&store, "queue/")), (2, 0));
}

/// Catches: a pin written over another writer's (the condition that it be absent dropped),
/// which queues one action twice when two writers look at the same moment. The other
/// writer's operation and pin land after this farm looked and before it wrote; the farm
/// is refused the write, looks again and joins that operation.
#[test]
fn a_pin_that_lands_after_the_look_is_not_written_over() {
    let store = LostAck::new();
    let farm = farm_over(&store);
    let other = durable_over(&store);
    let landing = other
        .submission(OperationId(5), &request(1), &subject(), None, None)
        .expect("encode")
        .put(
            format!("inflight/6d61696e/{}", request(1).key.dedup),
            5_u64.to_be_bytes(),
            Cond::Absent,
        );
    store.before_submit.lock().expect("lock").push_back(landing);

    assert!(submit(&farm, 1).is_ok());

    assert_eq!(rows(&store, "op/"), 1, "the action was queued twice");
    let ids: Vec<_> = unfinished(&farm).iter().map(|op| op.0).collect();
    assert_eq!(ids, [5]);
}

/// Catches: a pin that names an operation of another action being joined (the caller
/// would wait on the wrong run and be handed its result), and a row of the operation
/// that cannot be read being taken up as if it were whole: each is refused as corrupt.
#[test]
fn a_pin_or_row_that_cannot_be_trusted_is_refused() {
    let store = LostAck::new();
    let durable = durable_over(&store);
    assert!(
        durable
            .submit(OperationId(0), &request(2), &subject())
            .is_ok()
    );
    let pin = Txn::new().put(
        format!("inflight/6d61696e/{}", request(1).key.dedup),
        0_u64.to_be_bytes(),
        Cond::Absent,
    );
    store.txn(pin).expect("store").expect("put");
    let refused = durable.submit(OperationId(1), &request(1), &subject());
    assert!(
        matches!(refused, Err(StateError::Corrupt { .. })),
        "{refused:?}"
    );

    let bad_rows = [
        ("loss/0000000000000000", vec![1, 2, 3]),
        (
            "loss/0000000000000000",
            (1_u64 << 32).to_be_bytes().to_vec(),
        ),
        ("fate/0000000000000000", vec![255]),
        ("subject/0000000000000000", vec![255]),
    ];
    for (key, value) in bad_rows {
        let put = Txn::new().put(key, value, Cond::Any);
        store.txn(put).expect("store").expect("put");
        let refused = durable.submit(OperationId(1), &request(2), &subject());
        assert!(
            matches!(refused, Err(StateError::Corrupt { .. })),
            "{key}: {refused:?}"
        );
        let gone = Txn::new().delete(key, Cond::Any);
        store.txn(gone).expect("store").expect("delete");
    }
}

/// Catches: an operation that was queued and then requeued with losses and a fate being
/// joined without them (the joiner's farm would give it a fresh budget), and one with
/// none being refused.
#[test]
fn a_joined_operation_keeps_its_losses_and_fate() {
    let store = LostAck::new();
    let farm = farm_over(&store);
    let other = durable_over(&store);
    assert!(
        other
            .submit(OperationId(0), &request(1), &subject())
            .is_ok()
    );
    let start = kbf_types::StartLease {
        worker: kbf_types::WorkerId::new("node-a"),
        lease: kbf_types::LeaseId::new(5, 0),
        operation: OperationId(0),
        key: request(1).key,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        fence: kbf_types::FencePolicy::RunOn,
        device: None,
    };
    other.persist(&[Effect::Start(start)], None).expect("grant");
    let requeue = kbf_sched::Requeue {
        operation: OperationId(0),
        lease: kbf_types::LeaseId::new(5, 0),
        worker: kbf_types::WorkerId::new("node-a"),
        reason: kbf_sched::RequeueReason::Silent,
    };
    let fate = kbf_sched::Fate {
        booked_bytes: 1 << 30,
        not_started: 2,
        ..kbf_sched::Fate::default()
    };
    other
        .requeued(&requeue, Some(3), Some(&fate), &FloorChange::default())
        .expect("requeue");
    store
        .txn(Txn::new().delete("subject/0000000000000000", Cond::Any))
        .expect("store")
        .expect("delete");

    assert!(submit(&farm, 1).is_ok());

    let state = farm.lock();
    assert_eq!(state.sched.losses(OperationId(0)), Some(3));
    assert_eq!(state.sched.fate(OperationId(0)), Some(fate));
}

/// Asserts that a finish of an operation whose rows are gone (a retry of a finish that
/// landed) is acknowledged without a pin to look up, and writes nothing more: the result
/// of the first try stands.
#[test]
fn finishing_an_operation_with_no_rows_writes_nothing_more() {
    let store = LostAck::new();
    let durable = durable_over(&store);
    assert!(
        durable
            .submit(OperationId(7), &request(1), &subject())
            .is_ok()
    );
    refuse(&durable, 7);
    let txns = store.commit_stats().txns;

    refuse(&durable, 7);

    assert_eq!(store.commit_stats().txns, txns);
    assert_eq!(rows(&store, "inflight/"), 0);
    assert_eq!(rows(&store, "done/"), 1);
}

/// Catches: a store that is down at the write being taken for a refusal that stays (the
/// retry of a client after the outage is refused too) or leaving a row; and a farm
/// with no state store failing to queue in memory.
#[test]
fn a_store_that_is_down_refuses_the_submit_and_a_farm_without_one_queues() {
    let store = LostAck::new();
    let farm = farm_over(&store);
    store.down.store(true, Ordering::SeqCst);
    let refused = submit(&farm, 1);
    assert_eq!(refused.map(|_| ()).unwrap_err().code(), Code::Unavailable);
    assert_eq!(rows(&store, "op/"), 0);
    store.down.store(false, Ordering::SeqCst);
    assert!(submit(&farm, 1).is_ok());
    assert_eq!(rows(&store, "op/"), 1);

    let bare = Farm::new_legacy(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    );
    assert!(submit(&bare, 1).is_ok());
    assert_eq!(unfinished(&bare).len(), 1);
}

/// Asserts that a submit does not look again for ever when rows stay in the way (the ids of
/// its block all have rows of an earlier writer): it gives up, refused, after a few looks,
/// and writes nothing of its own.
#[test]
fn a_row_that_stays_in_the_way_ends_the_looks() {
    let store = LostAck::new();
    let farm = farm_over(&store);
    for id in 0..4 {
        let block = Txn::new().put(format!("op/{id:016x}"), Vec::new(), Cond::Any);
        store.txn(block).expect("store").expect("put");
    }

    let refused = submit(&farm, 1);

    assert_eq!(refused.map(|_| ()).unwrap_err().code(), Code::Unavailable);
    assert_eq!(rows(&store, "inflight/"), 0);
    assert_eq!(rows(&store, "queue/"), 0);
}

/// Catches: a second request for an action this farm already holds that writes to the
/// store again (a second operation, a second run) instead of joining the first; and a
/// farm superseded by a newer writer that queues work anyway, or answers a retryable
/// refusal where the client should stop.
#[test]
fn a_twin_in_memory_joins_without_a_write_and_a_fenced_farm_refuses_for_good() {
    let store = LostAck::new();
    let farm = farm_over(&store);
    assert!(submit(&farm, 1).is_ok());
    let writes = store.inner.commit_stats().txns;
    assert!(submit(&farm, 1).is_ok());
    assert_eq!(
        store.inner.commit_stats().txns,
        writes,
        "the twin was written"
    );
    assert_eq!(rows(&store, "op/"), 1);
    assert_eq!(unfinished(&farm).len(), 1);

    store.fenced.store(true, Ordering::SeqCst);
    let refused = submit(&farm, 2);
    assert_eq!(
        refused.map(|_| ()).unwrap_err().code(),
        Code::FailedPrecondition
    );
    assert_eq!(rows(&store, "op/"), 1);
}

/// Catches: a submit whose looks are not exactly `LOOKS` (3): one that gives up after the
/// first conflict (a busy store refuses work it could take), or one that looks without
/// end or once too often. Each of `landed` writers takes the id this submit tried just
/// before its write, so every try conflicts and the next takes the id after. Three
/// conflicts and the fourth try is taken, under the fourth id; four conflicts and the
/// submit is refused with the conflict, having written nothing of its own.
#[test]
fn a_submit_takes_three_conflicts_and_is_refused_at_the_fourth() {
    for (landed, taken) in [(3_u64, true), (4, false)] {
        let store = LostAck::new();
        let durable = durable_over(&store);
        let other = durable_over(&store);
        for id in 0..landed {
            let tag = 10 + u8::try_from(id).expect("a small id");
            let landing = other
                .submission(OperationId(id), &request(tag), &subject(), None, None)
                .expect("encode");
            store.before_submit.lock().expect("lock").push_back(landing);
        }

        let submitted = durable.submit(OperationId(0), &request(1), &subject());

        if taken {
            assert!(
                matches!(submitted, Ok(Submitted::Queued(OperationId(3)))),
                "{landed} conflicts: {submitted:?}"
            );
            assert_eq!(rows(&store, "inflight/"), 1);
        } else {
            assert!(
                matches!(submitted, Err(StateError::Conflict(_))),
                "{landed} conflicts: {submitted:?}"
            );
            assert_eq!(rows(&store, "inflight/"), 0, "a refused submit pinned");
        }
        assert_eq!(rows(&store, "op/"), landed as usize + usize::from(taken));
    }
}

/// Catches: a request that joins an operation the scheduler finished while the store still
/// holds its rows (the finish is held back): it would take the finished operation up
/// again and run the action twice. The scheduler finishes the queued operation (a
/// committed refusal, no store write), the rows stay, and the request is refused
/// UNAVAILABLE with nothing queued and no waiter attached. Once the rows go it queues a
/// fresh operation. This runs in the crate's own test binary, which also covers the rest
/// of `queue`, so that function's lines are all covered by one copy of it.
#[test]
fn a_request_for_an_operation_the_scheduler_finished_is_refused_until_the_rows_go() {
    let store = LostAck::new();
    let farm = farm_over(&store);
    assert!(submit(&farm, 1).is_ok());
    let refusal = kbf_types::ControlRecord::Refusal(kbf_types::RefusalRecord {
        operation: OperationId(0),
        reason: "no worker".to_owned(),
    });
    let input = kbf_sched::Input::new(
        kbf_types::FarmTime::from_millis(0),
        kbf_sched::Event::Committed(refusal),
    );
    let effects = kbf_types::StateMachine::apply(&mut farm.lock().sched, input);
    assert_eq!(effects.len(), 1, "the refusal was not taken");
    assert!(unfinished(&farm).is_empty());
    assert_eq!((rows(&store, "op/"), rows(&store, "inflight/")), (1, 1));

    let refused = submit(&farm, 1);

    assert_eq!(refused.map(|_| ()).unwrap_err().code(), Code::Unavailable);
    assert!(
        unfinished(&farm).is_empty(),
        "the finished operation came back"
    );
    let waiters = farm.lock().sched.waiters(OperationId(0)).map(<[_]>::len);
    assert_eq!(waiters, Some(1), "a waiter joined a finished operation");
    assert_eq!(rows(&store, "op/"), 1);

    let gone = Txn::new()
        .delete("op/0000000000000000", Cond::Any)
        .delete("queue/0000000000000000", Cond::Any)
        .delete("subject/0000000000000000", Cond::Any);
    store.txn(gone).expect("store").expect("delete");
    assert!(submit(&farm, 1).is_ok(), "the retry after the rows went");
    assert_eq!(unfinished(&farm), [OperationId(1)]);
}

/// Catches: a request that re-takes a finished operation the scheduler has already
/// retired while its finish is still held. The scheduler drops a finished operation after
/// `FINISHED_RETENTION` whether or not the finish was written, so `state()` no longer
/// guards; the held finish does. The request is refused UNAVAILABLE and nothing is queued
/// until the held finish goes, and then it queues afresh. Also covers every effect kind
/// `finish_held` looks at, in this crate's own test binary.
#[test]
fn a_request_is_refused_while_the_finish_of_a_retired_operation_is_held() {
    refused_while_the_finish_of_a_retired_operation_is_held(0);
}

/// Catches: the test above reading the retirement time off a clock of its own. The
/// scheduler takes the later of its own time and an input's, and the farm stamps its
/// submits with real elapsed milliseconds, so a slow `submit` moves the scheduler past the
/// time the refusal is applied at and the finish is stamped later than the test assumed.
/// Here the scheduler is 1 s in by the refusal, as after a submit that stalled.
#[test]
fn a_finish_stamped_after_a_slow_submit_is_retired_after_its_retention() {
    refused_while_the_finish_of_a_retired_operation_is_held(1000);
}

fn refused_while_the_finish_of_a_retired_operation_is_held(lag_millis: u64) {
    let store = LostAck::new();
    let farm = farm_over(&store);
    assert!(submit(&farm, 1).is_ok());
    let refusal = kbf_types::ControlRecord::Refusal(kbf_types::RefusalRecord {
        operation: OperationId(0),
        reason: "no worker".to_owned(),
    });
    let at = |millis, event| kbf_sched::Input::new(kbf_types::FarmTime::from_millis(millis), event);
    let effects = {
        let mut state = farm.lock();
        if lag_millis > 0 {
            kbf_types::StateMachine::apply(
                &mut state.sched,
                at(lag_millis, kbf_sched::Event::Tick),
            );
        }
        kbf_types::StateMachine::apply(
            &mut state.sched,
            at(0, kbf_sched::Event::Committed(refusal)),
        )
    };
    assert_eq!(effects.len(), 1);
    // The finish is stamped with the scheduler's own time: the later of the farm's clock
    // at the submits above and the lag. The farm clock only moves on, so a tick this far
    // past both retires it however long the submits took.
    let retention = u64::try_from(kbf_sched::FINISHED_RETENTION.as_millis()).expect("millis");
    let past = farm.now().as_millis().max(lag_millis) + retention;
    {
        let mut state = farm.lock();
        state.unwritten.push_back(
            [
                // An answer of another operation does not hold this one's rows.
                kbf_types::Effect::Answer(kbf_types::Answer {
                    operation: OperationId(7),
                    lease: kbf_types::LeaseId::new(5, 1),
                    waiters: Vec::new(),
                    outcome: kbf_types::Outcome::Failed(kbf_types::Failure::Invalid),
                    memory_runs: Vec::new(),
                    farm_runs: Vec::new(),
                }),
                kbf_types::Effect::Waiting(kbf_types::Waiting {
                    operation: OperationId(0),
                    reason: None,
                }),
                effects[0].clone(),
            ]
            .into(),
        );
        kbf_types::StateMachine::apply(&mut state.sched, at(past, kbf_sched::Event::Tick));
        assert!(state.sched.state(OperationId(0)).is_none(), "not retired");
    }
    assert_eq!((rows(&store, "op/"), rows(&store, "inflight/")), (1, 1));

    let refused = submit(&farm, 1);

    assert_eq!(refused.map(|_| ()).unwrap_err().code(), Code::Unavailable);
    assert!(unfinished(&farm).is_empty(), "the operation came back");

    farm.lock().unwritten.clear();
    let gone = Txn::new()
        .delete("op/0000000000000000", Cond::Any)
        .delete("queue/0000000000000000", Cond::Any)
        .delete("subject/0000000000000000", Cond::Any);
    store.txn(gone).expect("store").expect("delete");
    assert!(submit(&farm, 1).is_ok(), "the retry after the finish went");
    assert_eq!(unfinished(&farm), [OperationId(1)]);
}

/// Catches: an unpin of an operation whose rows are already gone failing the finish, or
/// putting a delete in its transaction.
#[test]
fn unpinning_an_operation_with_no_rows_changes_nothing() {
    let store = LostAck::new();
    let durable = durable_over(&store);
    let unpinned = durable.unpin(Txn::new(), OperationId(7)).expect("unpin");
    assert!(unpinned.ops().is_empty());
}
