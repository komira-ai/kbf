//! The rows the scheduler writes, read back as a start reads them.

use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, Ordering};

use kbf_caps::NodeCaps;
use kbf_log::{Bootstrap, FarmLogId};
use kbf_sched::RequeueReason;
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{
    Changes, Commit, CommitStats, Conflict, ManualClock, Revision, StoreTime, Versioned,
};
use kbf_types::{
    ActionKey, Answer, Digest, DigestFunction, Failure, FencePolicy, LeaseKind, Outcome, Qos,
    Refusal, RefusalRecord, Resources, Waiting,
};

use super::*;

/// A store whose writes can be made to fail.
struct Flaky {
    inner: Fake,
    down: AtomicBool,
}

impl Flaky {
    fn new() -> Arc<Self> {
        let inner = Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None);
        Arc::new(Self {
            inner,
            down: AtomicBool::new(false),
        })
    }

    fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }
}

impl Store for Flaky {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(StoreError::Failed("the store is down".to_owned()));
        }
        self.inner.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.inner.scan_prefix(prefix)
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

fn start(operation: u64, seq: u64) -> StartLease {
    StartLease {
        worker: WorkerId::new("node-a"),
        lease: LeaseId::new(5, seq),
        operation: OperationId(operation),
        key: request(1).key,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        fence: FencePolicy::RunOn,
        device: None,
    }
}

fn answer(operation: u64, seq: u64) -> Answer {
    Answer {
        operation: OperationId(operation),
        lease: LeaseId::new(5, seq),
        waiters: Vec::new(),
        outcome: Outcome::Failed(Failure::Invalid),
        memory_runs: Vec::new(),
        farm_runs: Vec::new(),
    }
}

fn requeue(operation: u64, seq: u64) -> Requeue {
    Requeue {
        operation: OperationId(operation),
        lease: LeaseId::new(5, seq),
        worker: WorkerId::new("node-a"),
        reason: RequeueReason::Silent,
    }
}

fn open(store: &Arc<Flaky>) -> Durable {
    let store: Arc<dyn Store> = store.clone();
    Durable::open(store, EpochPolicy::Keep, 100)
        .expect("open")
        .0
}

fn keys(store: &Flaky) -> Vec<String> {
    let rows = store.scan_prefix("").expect("scan");
    rows.into_iter().map(|(key, _)| key).collect()
}

fn put(store: &Flaky, key: &str, value: Vec<u8>) {
    let put = Txn::new().put(key, value, Cond::Any);
    store.txn(put).expect("store").expect("put");
}

/// Catches: an epoch that is not kept across starts that keep it, not raised by one that
/// bumps it (a daemon would keep the runs of leases from before a restored file), a new
/// store that does not start from the seed it is given (two stores could both begin at
/// 1 and a daemon would take one for the other), and an epoch that is not durable.
#[test]
fn the_epoch_is_seeded_kept_and_bumped() {
    let store = Flaky::new();
    let dyn_store = || -> Arc<dyn Store> { store.clone() };
    let epoch = |policy, seed| Durable::open(dyn_store(), policy, seed).expect("open").1;
    assert_eq!(epoch(EpochPolicy::Keep, 100), 100);
    assert_eq!(
        epoch(EpochPolicy::Keep, 5),
        100,
        "a kept epoch is the store's"
    );
    assert_eq!(epoch(EpochPolicy::Bump, 5), 101);
    assert_eq!(epoch(EpochPolicy::Keep, 5), 101, "a bump is durable");
    assert_eq!(epoch(EpochPolicy::Bump, 5), 102);
}

/// Catches: an operation not written at submit (a restart forgets work the client was
/// told is queued), its place in the queue or its next id not kept, and a second write
/// of the same operation accepted over the first.
#[test]
fn a_submitted_operation_is_loaded_queued() {
    let store = Flaky::new();
    let durable = open(&store);
    durable
        .submitted(OperationId(4), &request(1))
        .expect("submit");
    let stored = durable.load().expect("load");
    assert_eq!(
        stored,
        Stored {
            next_operation: 5,
            next_seq: 0,
            operations: vec![StoredOperation {
                operation: OperationId(4),
                request: request(1),
                lease: None,
            }],
        }
    );
    let again = durable.submitted(OperationId(4), &request(2));
    assert!(matches!(again, Err(StateError::Conflict(_))), "{again:?}");
}

/// Catches: a grant whose lease is not in the store (a restart forgets the claim a
/// daemon was told), whose operation stays queued, or whose lease sequence is not
/// kept (a restart would grant the lease id again: a stale daemon's result taken as
/// the new lease's).
#[test]
fn a_grant_holds_the_lease_and_leaves_the_queue() {
    let store = Flaky::new();
    let durable = open(&store);
    durable
        .submitted(OperationId(4), &request(1))
        .expect("submit");
    durable
        .persist(&[Effect::Start(start(4, 7))])
        .expect("grant");
    let stored = durable.load().expect("load");
    assert_eq!(stored.next_seq, 8);
    let [held] = stored.operations.as_slice() else {
        panic!("{stored:?}");
    };
    let grant = held.lease.as_ref().expect("a lease");
    assert_eq!(grant.lease, LeaseId::new(5, 7));
    assert_eq!(grant.worker, WorkerId::new("node-a"));
    assert!(!keys(&store).iter().any(|k| k.starts_with("queue/")));
}

/// Catches: a second live lease for one operation (two daemons run it and either
/// result is accepted): a grant needs the operation queued, which the first grant ended.
#[test]
fn an_operation_never_holds_two_leases() {
    let store = Flaky::new();
    let durable = open(&store);
    durable
        .submitted(OperationId(4), &request(1))
        .expect("submit");
    durable
        .persist(&[Effect::Start(start(4, 0))])
        .expect("grant");
    let second = durable.persist(&[Effect::Start(start(4, 1))]);
    assert!(matches!(second, Err(StateError::Missing(_))), "{second:?}");
    let stored = durable.load().expect("load");
    let lease = stored.operations[0].lease.as_ref().map(|g| g.lease);
    assert_eq!(lease, Some(LeaseId::new(5, 0)));
}

/// Catches: a result accepted from a lease the operation no longer holds: the store must
/// compare-and-set on the operation's lease, so a stale `complete` (the first lease's,
/// given up and granted again) is refused and the operation keeps its second lease.
#[test]
fn a_stale_complete_is_rejected() {
    let store = Flaky::new();
    let durable = open(&store);
    durable
        .submitted(OperationId(4), &request(1))
        .expect("submit");
    durable
        .persist(&[Effect::Start(start(4, 0))])
        .expect("first grant");
    durable.requeued(&requeue(4, 0)).expect("given up");
    durable
        .persist(&[Effect::Start(start(4, 1))])
        .expect("second grant");

    let stale = durable.persist(&[Effect::Answer(answer(4, 0))]);
    assert!(matches!(stale, Err(StateError::Missing(_))), "{stale:?}");
    let stored = durable.load().expect("load");
    let held = stored.operations[0].lease.as_ref().map(|g| g.lease);
    assert_eq!(
        held,
        Some(LeaseId::new(5, 1)),
        "the stale result moved the operation"
    );

    durable
        .persist(&[Effect::Answer(answer(4, 1))])
        .expect("the current lease's result");
    let stored = durable.load().expect("load");
    assert!(stored.operations.is_empty());
    assert_eq!(
        stored.next_seq, 2,
        "a finished operation's ids are not reused"
    );
    assert_eq!(
        keys(&store),
        ["meta/epoch", "meta/next_op", "meta/next_seq"]
    );
}

/// Catches: a lease given up that stays in the store (a restart re-binds a run nobody
/// holds), an operation not queued again after it, and a lease given up twice.
#[test]
fn a_given_up_lease_requeues_its_operation_once() {
    let store = Flaky::new();
    let durable = open(&store);
    durable
        .submitted(OperationId(4), &request(1))
        .expect("submit");
    durable
        .persist(&[Effect::Start(start(4, 0))])
        .expect("grant");
    durable.requeued(&requeue(4, 0)).expect("given up");
    let stored = durable.load().expect("load");
    assert_eq!(stored.operations[0].lease, None);
    assert!(keys(&store).contains(&"queue/0000000000000004".to_owned()));
    let twice = durable.requeued(&requeue(4, 0));
    assert!(matches!(twice, Err(StateError::Missing(_))), "{twice:?}");
}

/// Catches: a refused operation left in the store (it would be refused, or run, again
/// after a restart), and effects that settle nothing (a wait notice, a proposed record)
/// writing anything.
#[test]
fn a_refusal_removes_the_operation_and_other_effects_write_nothing() {
    let store = Flaky::new();
    let durable = open(&store);
    durable
        .submitted(OperationId(4), &request(1))
        .expect("submit");
    let before = store.commit_stats().txns;
    let unsettled = [
        Effect::Waiting(Waiting {
            operation: OperationId(4),
            reason: None,
        }),
        Effect::Commit(ControlRecord::Refusal(RefusalRecord {
            operation: OperationId(4),
            reason: "no worker".to_owned(),
        })),
    ];
    durable.persist(&unsettled).expect("nothing to write");
    assert_eq!(store.commit_stats().txns, before);
    assert_eq!(durable.load().expect("load").operations.len(), 1);

    let refusal = Refusal {
        operation: OperationId(4),
        waiters: Vec::new(),
        reason: "no worker".to_owned(),
    };
    durable
        .persist(&[Effect::Refuse(refusal)])
        .expect("refused");
    assert!(durable.load().expect("load").operations.is_empty());
    assert_eq!(keys(&store), ["meta/epoch", "meta/next_op"]);
}

/// Catches: a registration that leaves no session row, or one a start could not read
/// back as what the scheduler was told (instance, capacity).
#[test]
fn a_registration_writes_the_session_row() {
    let store = Flaky::new();
    let durable = open(&store);
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")]).expect("caps");
    let reported = Reported::serving(Resources::new(2_000, 1 << 30), caps);
    let worker = WorkerId::new("node-a");
    let instance = DaemonInstance::new("daemon-1");
    durable
        .session(&worker, &instance, true, &reported)
        .expect("session");
    let row = store.get("session/node-a").expect("get").expect("a row");
    let FarmInput::WorkerUp {
        instance: stored,
        capacity,
        predecessor_stopped,
        ..
    } = decode("session/node-a", &row.value).expect("decode")
    else {
        panic!("not a registration");
    };
    assert_eq!(stored, instance);
    assert_eq!(capacity, Resources::new(2_000, 1 << 30));
    assert!(predecessor_stopped);
}

/// Catches: a row the scheduler cannot read skipped (work silently lost at a restart)
/// instead of refused, and a refusal that does not name the row.
#[test]
fn an_unreadable_row_is_refused_naming_it() {
    let lease = encode(FarmInput::Committed(ControlRecord::Lease(LeaseGrant {
        lease: LeaseId::new(5, 0),
        operation: OperationId(1),
        worker: WorkerId::new("node-a"),
    })))
    .expect("encode");
    let submit = encode(FarmInput::Submit(request(1))).expect("encode");
    let other = kbf_log::encode(
        Format::V1,
        &LogCommand::Bootstrap(Bootstrap {
            log_id: FarmLogId::new(NonZeroU64::new(1).expect("non-zero")),
            store_prefix: "kbf/".to_owned(),
        }),
    )
    .expect("encode");
    let pair = LogCommand::Farm(FarmBatch {
        now: FarmTime::default(),
        wall: UNIX_EPOCH,
        inputs: vec![FarmInput::Tick, FarmInput::Tick],
    });
    let pair = kbf_log::encode(Format::V1, &pair).expect("encode");
    let rows: [(&str, Vec<u8>, &str); 8] = [
        ("op/0000000000000001", vec![0xff, 0xff, 0xff], "malformed"),
        ("op/0000000000000001", lease.clone(), "not a request"),
        ("op/0000000000000001", other, "not a single scheduler input"),
        ("op/0000000000000001", pair, "not a single scheduler input"),
        ("op/zz", submit.clone(), "not an operation id"),
        (
            "lease/0000000000000005.0000000000000000",
            submit,
            "not a lease grant",
        ),
        ("meta/next_op", vec![1, 2, 3], "not an 8-byte number"),
        ("meta/next_seq", vec![1, 2, 3], "not an 8-byte number"),
    ];
    for (key, value, why) in rows {
        let store = Flaky::new();
        let durable = open(&store);
        put(&store, key, value);
        let error = durable.load().expect_err(key);
        let text = error.to_string();
        assert!(text.contains(key) && text.contains(why), "{key}: {text}");
    }
    let store = Flaky::new();
    put(&store, "meta/epoch", vec![1]);
    let dyn_store: Arc<dyn Store> = store.clone();
    let opened = Durable::open(dyn_store, EpochPolicy::Keep, 1);
    assert!(matches!(opened, Err(StateError::Corrupt { .. })));
}

/// Catches: a store failure turned into success (an acknowledgement for a write that is
/// not durable), at open, at read and at write.
#[test]
fn a_failing_store_fails_the_write() {
    let store = Flaky::new();
    let durable = open(&store);
    store.set_down(true);
    let submit = durable.submitted(OperationId(1), &request(1));
    assert!(matches!(submit, Err(StateError::Store(_))), "{submit:?}");
    let dyn_store: Arc<dyn Store> = store.clone();
    let opened = Durable::open(dyn_store, EpochPolicy::Bump, 1);
    assert!(matches!(opened, Err(StateError::Store(_))));
    store.set_down(false);
    durable
        .submitted(OperationId(1), &request(1))
        .expect("up again");
}

/// Catches: a store wrapper that answers reads and the feed from somewhere but the fake
/// it wraps (the tests above would pass over an empty store), and a handle that does not
/// say what it is when a farm holding it is printed.
#[test]
fn the_flaky_store_reads_through_and_the_handle_prints() {
    let store = Flaky::new();
    let durable = open(&store);
    durable
        .submitted(OperationId(1), &request(1))
        .expect("submit");
    assert_eq!(store.reap_expired(), Ok(0));
    assert_eq!(store.now(), StoreTime::ZERO);
    let changes = store.changes(Revision::ZERO, 100).expect("changes");
    let written = changes
        .rows
        .iter()
        .any(|row| row.key == "op/0000000000000001");
    assert!(written);
    assert_eq!(format!("{durable:?}"), "Durable");
}
