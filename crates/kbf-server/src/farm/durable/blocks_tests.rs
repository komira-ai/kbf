//! The ids a frontend numbers with, and the epoch it serves under, when several frontends
//! share one state store: blocks of operation ids and lease sequences reserved with a
//! compare-and-set on the counter rows, an epoch created once and kept, and the operator's
//! raise of it.

use std::sync::atomic::AtomicUsize;

use kbf_front::{Cache, Dispatch, Submission};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{
    Changes, Commit, CommitStats, ManualClock, Revision, ScanPage, ScanRange, StoreTime,
};
use kbf_types::{ActionKey, DigestFunction, LeaseKind, Qos, Resources};

use tonic::Code;

use super::*;

const CAS_TRIES: usize = super::blocks::CAS_TRIES;

/// A store that applies a transaction of another frontend just before a write of this one
/// that names the transaction's key, oldest first: a write landing between this
/// frontend's read of a row and its own write of it.
pub(super) struct Racing {
    inner: Fake,
    clock: Arc<ManualClock>,
    landings: Mutex<Vec<(String, Txn)>>,
    down: AtomicBool,
    lease_scans: AtomicUsize,
    session_scans: AtomicUsize,
    /// Rows whose key starts with one of these cannot be read.
    unreadable: Mutex<Vec<String>>,
    /// Transactions that name a row whose key starts with one of these are refused.
    unwritable: Mutex<Vec<String>>,
}

impl Racing {
    pub(super) fn new() -> Arc<Self> {
        let clock = Arc::new(ManualClock::default());
        Arc::new(Self {
            inner: Fake::with_flaw(Arc::clone(&clock), Flaw::None),
            clock,
            landings: Mutex::new(Vec::new()),
            down: AtomicBool::new(false),
            lease_scans: AtomicUsize::new(0),
            session_scans: AtomicUsize::new(0),
            unreadable: Mutex::new(Vec::new()),
            unwritable: Mutex::new(Vec::new()),
        })
    }

    /// How many times every `session/` row has been read by a scan.
    pub(super) fn session_scans(&self) -> usize {
        self.session_scans.load(Ordering::SeqCst)
    }

    /// How many times every `lease/` row has been read by a scan.
    pub(super) fn lease_scans(&self) -> usize {
        self.lease_scans.load(Ordering::SeqCst)
    }

    /// Moves the store's clock forward by `by`.
    pub(super) fn advance(&self, by: Duration) {
        self.clock.advance(by);
    }

    /// The store takes writes again, or not.
    pub(super) fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }

    /// Reads of the rows under `prefix` fail from now on, or work again (`None`).
    pub(super) fn fail_reads(&self, prefix: Option<&str>) {
        let mut unreadable = self.unreadable.lock().expect("lock");
        unreadable.clear();
        unreadable.extend(prefix.map(str::to_owned));
    }

    /// Writes to the rows under `prefix` fail from now on, or work again (`None`).
    pub(super) fn fail_writes(&self, prefix: Option<&str>) {
        let mut unwritable = self.unwritable.lock().expect("lock");
        unwritable.clear();
        unwritable.extend(prefix.map(str::to_owned));
    }

    fn unreadable(&self, key: &str) -> Result<(), StoreError> {
        let unreadable = self.unreadable.lock().expect("lock");
        let failing = unreadable.iter().any(|prefix| key.starts_with(prefix));
        let failed = || StoreError::Failed("the row cannot be read".to_owned());
        failing.then(failed).map_or(Ok(()), Err)
    }

    /// `landing` is applied before the next write that names `key`.
    pub(super) fn land(&self, key: &str, landing: Txn) {
        let mut queued = self.landings.lock().expect("lock");
        queued.push((key.to_owned(), landing));
    }
}

impl Store for Racing {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(StoreError::Failed("the store is down".to_owned()));
        }
        let refused = self.unwritable.lock().expect("lock");
        let writes_refused = batch
            .iter()
            .flat_map(Txn::ops)
            .filter_map(|op| op.key_cond())
            .any(|(key, _)| refused.iter().any(|prefix| key.starts_with(prefix)));
        drop(refused);
        if writes_refused {
            return Err(StoreError::Failed("the row cannot be written".to_owned()));
        }
        let names = |key: &str| {
            let mut ops = batch.iter().flat_map(Txn::ops);
            ops.any(|op| op.key_cond().is_some_and(|(named, _)| named == key))
        };
        let mut queued = self.landings.lock().expect("lock");
        if let Some(at) = queued.iter().position(|(key, _)| names(key)) {
            let (_, landing) = queued.remove(at);
            self.inner.txn(landing)?.expect("the landing write");
        }
        drop(queued);
        self.inner.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.unreadable(key)?;
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.unreadable(prefix)?;
        let scans = &self.lease_scans;
        scans.fetch_add(usize::from(prefix == "lease/"), Ordering::SeqCst);
        self.session_scans
            .fetch_add(usize::from(prefix == "session/"), Ordering::SeqCst);
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

/// A handle over `store` that kept the epoch it found or made (100 in an empty store).
pub(super) fn durable_over(store: &Arc<Racing>) -> Durable {
    let store: Arc<dyn Store> = store.clone();
    Durable::open(store, EpochPolicy::Keep, 100)
        .expect("open")
        .0
}

fn counter(store: &Racing, key: &str) -> u64 {
    let row = store.get(key).expect("read").expect("a row");
    u64::from_be_bytes(row.value.as_slice().try_into().expect("eight bytes"))
}

/// A write of `value` to the counter row `key`, whatever it holds.
fn set(key: &str, value: u64) -> Txn {
    Txn::new().put(key, value.to_be_bytes(), Cond::Any)
}

fn bump(previous: u64, epoch: u64) -> EpochBump {
    EpochBump { previous, epoch }
}

/// The action `request(tag)` runs.
pub(super) fn action_key(tag: u8) -> ActionKey {
    ActionKey::try_from(&request(tag).key).expect("an action's key")
}

pub(super) fn request(tag: u8) -> Request {
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

fn try_farm_over(store: &Arc<Racing>) -> Result<Farm, StateError> {
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
}

pub(super) fn farm_over(store: &Arc<Racing>) -> Farm {
    try_farm_over(store).expect("a farm over the store")
}

pub(super) fn submit(farm: &Farm, tag: u8) -> Result<(), Status> {
    farm.submit(Submission {
        request: request(tag),
        call: None,
        described: Described::default(),
        evidence: Default::default(),
        max_wall: None,
    })
    .map(drop)
}

pub(super) fn unfinished(farm: &Farm) -> Vec<OperationId> {
    farm.lock().sched.unfinished().collect()
}

/// Asserts that a reservation starts at the stored counter or at the frontend's own next
/// id, whichever is later, ends [`ID_BLOCK`] past its start and writes its end: the
/// stored counter never goes back, whatever the frontend's own next id is.
#[test]
fn a_reservation_starts_at_the_later_of_the_counter_and_the_own_next_id() {
    let store = Racing::new();
    let durable = durable_over(&store);
    let first = durable.reserve_seqs(0).expect("a first block");
    assert_eq!(first, 0..ID_BLOCK);
    assert_eq!(counter(&store, NEXT_SEQ), ID_BLOCK);

    // An own next id behind the counter: the frontend's memory is stale.
    let stale = durable.reserve_seqs(3).expect("a block");
    assert_eq!(stale, ID_BLOCK..2 * ID_BLOCK);

    // An own next id ahead of it.
    let ahead = 5 * ID_BLOCK + 7;
    let block = durable.reserve_seqs(ahead).expect("a block");
    assert_eq!(block, ahead..ahead + ID_BLOCK);
    assert_eq!(counter(&store, NEXT_SEQ), ahead + ID_BLOCK);

    let stale = durable.reserve_seqs(0).expect("a block");
    assert_eq!(stale.start, ahead + ID_BLOCK);
    assert!(store.get(NEXT_OP).expect("read").is_none());
}

/// Asserts that blocks reserved through several handles over one store never overlap, in
/// whatever order they ask and whatever their own next ids are: each starts where the one
/// before it ended.
#[test]
fn blocks_of_several_handles_over_one_store_never_overlap() {
    let store = Racing::new();
    let handles = [
        durable_over(&store),
        durable_over(&store),
        durable_over(&store),
    ];
    let mut blocks = Vec::new();
    for round in 0..4_u64 {
        for (own, handle) in (0..).zip(&handles) {
            blocks.push(handle.reserve_seqs(round * own).expect("a block"));
        }
    }
    assert_eq!(blocks.len(), 12);
    for pair in blocks.windows(2) {
        assert_eq!(pair[1].start, pair[0].end, "{blocks:?}");
    }
    assert_eq!(counter(&store, NEXT_SEQ), 12 * ID_BLOCK);
}

/// Asserts that a reservation which loses the compare-and-set to another frontend reads
/// the counter again and reserves past the block the other took, whether the counter did
/// not exist when it read it or held an older value.
#[test]
fn a_reservation_that_loses_a_race_reserves_past_the_winners_block() {
    let store = Racing::new();
    let durable = durable_over(&store);
    store.land(NEXT_SEQ, set(NEXT_SEQ, 5_000));

    let block = durable.reserve_seqs(0).expect("a block");

    assert_eq!(block, 5_000..5_000 + ID_BLOCK);
    assert_eq!(counter(&store, NEXT_SEQ), 5_000 + ID_BLOCK);
    store.land(NEXT_SEQ, set(NEXT_SEQ, 9_000));

    let block = durable.reserve_seqs(0).expect("a block");

    assert_eq!(block, 9_000..9_000 + ID_BLOCK);
}

/// Asserts that a reservation makes a bounded number of tries: it is taken on its last
/// try, and refused with the conflict after losing every one, having written nothing of
/// its own.
#[test]
fn a_reservation_gives_up_after_its_tries() {
    for (lost, taken) in [(CAS_TRIES - 1, true), (CAS_TRIES, false)] {
        let store = Racing::new();
        let durable = durable_over(&store);
        let mut last = 0;
        for n in 1..=lost {
            last = 100 * u64::try_from(n).expect("a small number");
            store.land(NEXT_SEQ, set(NEXT_SEQ, last));
        }

        let reserved = durable.reserve_seqs(0);

        if taken {
            assert_eq!(reserved.expect("taken"), last..last + ID_BLOCK);
        } else {
            assert!(
                matches!(reserved, Err(StateError::Conflict(_))),
                "{lost} lost: {reserved:?}"
            );
            assert_eq!(counter(&store, NEXT_SEQ), last, "a refused try wrote");
        }
    }
}

/// Asserts that an operation id inside the block the handle reserved is used as it is, with
/// no write; that one ahead of the block is numbered in a block reserved at it; and that one
/// behind the block (a scheduler that did not move on after a submit that failed) is
/// numbered at the start of the block the handle holds, with no write and no new block.
#[test]
fn an_operation_id_outside_the_reserved_block_is_numbered_in_a_new_block() {
    let store = Racing::new();
    let durable = durable_over(&store);
    let first = durable.reserve_operation(OperationId(4)).expect("an id");
    assert_eq!(first, OperationId(4));
    let writes = store.commit_stats().txns;

    let inside = durable
        .reserve_operation(OperationId(ID_BLOCK + 3))
        .expect("an id");

    assert_eq!(inside, OperationId(ID_BLOCK + 3));
    let now = store.commit_stats().txns;
    assert_eq!(now, writes, "an id in the block wrote");
    let ahead = durable
        .reserve_operation(OperationId(3 * ID_BLOCK))
        .expect("an id");
    assert_eq!(ahead, OperationId(3 * ID_BLOCK));
    let writes = store.commit_stats().txns;
    let behind = durable.reserve_operation(OperationId(4)).expect("an id");
    assert_eq!(behind, OperationId(3 * ID_BLOCK));
    let now = store.commit_stats().txns;
    assert_eq!(now, writes, "an id behind the block wrote");
    assert_eq!(counter(&store, NEXT_OP), 4 * ID_BLOCK);
}

/// Asserts that an operation id is refused, not guessed, when the block cannot be
/// reserved: another frontend wins every try, and the error is the conflict.
#[test]
fn an_operation_id_is_refused_when_every_try_to_reserve_its_block_is_lost() {
    let store = Racing::new();
    let durable = durable_over(&store);
    for n in 1..=CAS_TRIES {
        let taken = ID_BLOCK * u64::try_from(n).expect("a small number");
        store.land(NEXT_OP, set(NEXT_OP, taken));
    }

    let refused = durable.reserve_operation(OperationId(0));

    assert!(
        matches!(refused, Err(StateError::Conflict(_))),
        "{refused:?}"
    );
}

/// Asserts that a frontend that loses the race to create the epoch of an empty store
/// reads the winner's and keeps it: the two end with one epoch.
#[test]
fn the_frontend_that_loses_the_race_to_make_the_epoch_keeps_the_winners() {
    let store = Racing::new();
    store.land(EPOCH, set(EPOCH, 777));
    let dyn_store: Arc<dyn Store> = store.clone();

    let (_, epoch) = Durable::open(dyn_store, EpochPolicy::Keep, 100).expect("open");

    assert_eq!(epoch, 777);
    assert_eq!(counter(&store, EPOCH), 777);
}

/// Asserts that a bump that loses to another writer bumps what it reads next, so the
/// epoch is raised by one from the value the other left, for the operator's bump and for
/// an open that bumps alike.
#[test]
fn a_bump_that_loses_a_race_bumps_what_it_reads_next() {
    let store = Racing::new();
    let durable = durable_over(&store);
    store.land(EPOCH, set(EPOCH, 500));

    let bumped = durable.bump_epoch().expect("a bump");

    assert_eq!(bumped, bump(500, 501));
    assert_eq!(counter(&store, EPOCH), 501);
    store.land(EPOCH, set(EPOCH, 900));
    let dyn_store: Arc<dyn Store> = store.clone();

    let (_, epoch) = Durable::open(dyn_store, EpochPolicy::Bump, 1).expect("open");

    assert_eq!(epoch, 901);
}

/// Asserts that an open that loses every try to settle the epoch is refused with the
/// conflict, not left to try for ever.
#[test]
fn an_open_that_loses_every_try_is_refused_with_the_conflict() {
    let store = Racing::new();
    drop(durable_over(&store));
    for n in 0..CAS_TRIES {
        let raised = 200 + u64::try_from(n).expect("a small number");
        store.land(EPOCH, set(EPOCH, raised));
    }
    let dyn_store: Arc<dyn Store> = store.clone();

    let refused = Durable::open(dyn_store, EpochPolicy::Bump, 1).err();

    assert!(
        matches!(refused, Some(StateError::Conflict(_))),
        "{refused:?}"
    );
}

/// Asserts that a bump raises the stored epoch by one each time and is read by the next
/// handle, and that a store whose epoch row is not a number, or is gone, refuses it.
#[test]
fn a_bump_raises_the_stored_epoch_by_one_each_time() {
    let store = Racing::new();
    let durable = durable_over(&store);

    let first = durable.bump_epoch().expect("a bump");
    let second = durable.bump_epoch().expect("a bump");

    assert_eq!(first, bump(100, 101));
    assert_eq!(second, bump(101, 102));
    let (_, epoch) = Durable::open(store.clone(), EpochPolicy::Keep, 1).expect("open");
    assert_eq!(epoch, 102);
    let junk = Txn::new().put(EPOCH, vec![1], Cond::Any);
    store.txn(junk).expect("store").expect("put");
    let corrupt = durable.bump_epoch();
    assert!(
        matches!(corrupt, Err(StateError::Corrupt { .. })),
        "{corrupt:?}"
    );
    let gone = Txn::new().delete(EPOCH, Cond::Any);
    store.txn(gone).expect("store").expect("delete");
    let missing = durable.bump_epoch();
    assert!(
        matches!(missing, Err(StateError::Missing(_))),
        "{missing:?}"
    );
}

/// Asserts that each farm over a store numbers its operations in a block no other farm
/// has: two farms over one store submit operations whose ids are a block apart.
#[test]
fn farms_over_one_store_number_their_operations_in_blocks_of_their_own() {
    let store = Racing::new();
    let (first, second) = (farm_over(&store), farm_over(&store));

    submit(&first, 1).expect("queued");
    submit(&second, 2).expect("queued");

    assert_eq!(unfinished(&first), [OperationId(0)]);
    assert_eq!(unfinished(&second), [OperationId(ID_BLOCK)]);
    assert_eq!(store.scan_prefix("op/").expect("scan").len(), 2);
}

/// Asserts that a farm started over a store whose first operation finished and whose rows
/// are gone numbers its first operation past the block the earlier farm reserved: the
/// absence of an `op/` row guards no id.
#[test]
fn a_restarted_farm_never_numbers_an_operation_like_a_finished_one() {
    let store = Racing::new();
    let before = farm_over(&store);
    submit(&before, 1).expect("queued");
    assert_eq!(unfinished(&before), [OperationId(0)]);
    let refusal = kbf_types::Refusal {
        operation: OperationId(0),
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
    durable_over(&store)
        .persist(&[Effect::Refuse(refusal)], Some(&ending))
        .expect("refused");
    assert!(store.scan_prefix("op/").expect("scan").is_empty());
    drop(before);

    let after = farm_over(&store);
    submit(&after, 1).expect("queued");

    assert_eq!(unfinished(&after), [OperationId(ID_BLOCK)]);
}

/// Asserts that a farm over a store whose operation block is lost to other frontends on
/// every try is refused at its start, and so is one whose lease sequences are: it is not
/// started with ids that another frontend may number with.
#[test]
fn a_farm_whose_first_block_cannot_be_reserved_is_refused() {
    for key in [NEXT_OP, NEXT_SEQ] {
        let store = Racing::new();
        for n in 1..=CAS_TRIES {
            let taken = ID_BLOCK * u64::try_from(n).expect("a small number");
            store.land(key, set(key, taken));
        }

        let refused = try_farm_over(&store).err();

        assert!(
            matches!(refused, Some(StateError::Conflict(_))),
            "{key}: {refused:?}"
        );
    }
}

/// Asserts that a submit whose block of operation ids cannot be reserved is refused
/// UNAVAILABLE and queues nothing: the id is never guessed.
#[test]
fn a_submit_that_cannot_reserve_its_id_queues_nothing() {
    let store = Racing::new();
    let farm = farm_over(&store);
    {
        let state = farm.lock();
        let durable = state.durable.as_ref().expect("a store");
        *durable.ops.lock().expect("lock") = 0..0;
    }
    for n in 1..=CAS_TRIES {
        let taken = 10 * ID_BLOCK * u64::try_from(n).expect("a small number");
        store.land(NEXT_OP, set(NEXT_OP, taken));
    }

    let refused = submit(&farm, 1).expect_err("not queued");

    assert_eq!(refused.code(), Code::Unavailable);
    assert!(store.scan_prefix("op/").expect("scan").is_empty());
    assert!(unfinished(&farm).is_empty());
}

/// Asserts that a scheduler with room in its block of lease sequences is given no other:
/// a feed reserves nothing while at least two placement rounds are left.
#[test]
fn a_block_with_room_is_not_replaced() {
    let store = Racing::new();
    let farm = farm_over(&store);
    assert_eq!(counter(&store, NEXT_SEQ), ID_BLOCK);
    let writes = store.commit_stats().txns;

    farm.lock().reserve_seqs();

    let now = store.commit_stats().txns;
    assert_eq!(now, writes, "a block with room was replaced");
    assert_eq!(farm.lock().sched.seqs_left(), ID_BLOCK);
}

/// Asserts that a scheduler whose block of lease sequences runs low is given the next
/// block the store has, and numbers from its start (what was left of the old block is not
/// used).
#[test]
fn a_block_that_runs_low_is_replaced_by_the_next_one() {
    let store = Racing::new();
    let farm = farm_over(&store);
    farm.lock().sched.use_seq_block(1_000..ID_BLOCK);
    assert_eq!(farm.lock().sched.seqs_left(), ID_BLOCK - 1_000);

    farm.lock().reserve_seqs();

    let state = farm.lock();
    assert_eq!(state.sched.next_seq(), ID_BLOCK);
    assert_eq!(state.sched.seqs_left(), ID_BLOCK);
    assert_eq!(counter(&store, NEXT_SEQ), 2 * ID_BLOCK);
}

/// Asserts that nothing is reserved while the store is known to be down, and that a
/// reservation the store refuses leaves the block as it was and is tried at the next feed.
#[test]
fn a_reservation_waits_for_the_store() {
    let store = Racing::new();
    let farm = farm_over(&store);
    farm.lock().sched.use_seq_block(1_000..ID_BLOCK);
    farm.lock().store_down = true;

    farm.lock().reserve_seqs();

    assert_eq!(farm.lock().sched.seqs_left(), ID_BLOCK - 1_000);
    assert_eq!(counter(&store, NEXT_SEQ), ID_BLOCK, "reserved while down");
    farm.lock().store_down = false;
    store.down.store(true, Ordering::SeqCst);

    farm.lock().reserve_seqs();

    assert_eq!(farm.lock().sched.seqs_left(), ID_BLOCK - 1_000);
    store.down.store(false, Ordering::SeqCst);

    farm.lock().reserve_seqs();

    assert_eq!(farm.lock().sched.seqs_left(), ID_BLOCK);
    assert_eq!(counter(&store, NEXT_SEQ), 2 * ID_BLOCK);
}

/// Asserts that a farm over a store whose epoch another frontend made first keeps that
/// epoch as its term and its lease epoch.
#[test]
fn a_farm_started_as_the_epoch_was_made_serves_that_epoch() {
    let store = Racing::new();
    store.land(EPOCH, set(EPOCH, 777));

    let farm = farm_over(&store);

    assert_eq!(farm.lease_epoch(), 777);
    assert_eq!(farm.term(), 777);
}

/// Asserts that the operator's bump raises the stored epoch by one and not the epoch
/// this farm serves, that a farm started after it serves the new one, and that a farm
/// with no state store has no epoch to raise.
#[test]
fn a_bump_through_a_farm_is_served_by_the_next_start() {
    let store = Racing::new();
    let farm = farm_over(&store);
    let epoch = farm.lease_epoch();

    let bumped = farm.bump_epoch().expect("a bump");

    assert_eq!(bumped, Some(bump(epoch, epoch + 1)));
    assert_eq!(farm.lease_epoch(), epoch);
    assert_eq!(counter(&store, EPOCH), epoch + 1);
    assert_eq!(farm_over(&store).lease_epoch(), epoch + 1);
    let bare = Farm::new_legacy(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    );
    assert_eq!(bare.bump_epoch().expect("no store"), None);
}
