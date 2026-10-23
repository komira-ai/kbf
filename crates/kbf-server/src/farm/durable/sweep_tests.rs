//! The orphan sweep: a lease whose node no frontend hears is requeued once two looks at the
//! store, [`ORPHAN_WAIT`] apart on the store's clock, found it so. Each test stands a handle
//! over a store whose clock the test moves.

use std::collections::BTreeSet;

use kbf_caps::NodeCaps;
use kbf_types::Resources;

use super::blocks_tests::{Racing, durable_over, request};
use super::owner::pair_row;
use super::owner_tests::{grant, overwrite, remove, value_of};
use super::transition_tests::{rows, subject};
use super::*;

const WAIT: Duration = Duration::from_secs(30);

fn node() -> WorkerId {
    WorkerId::new("node-a")
}

fn at(secs: u64) -> FarmTime {
    FarmTime::from_millis(secs * 1_000)
}

fn reported() -> Reported {
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")]).expect("caps");
    Reported::serving(Resources::new(2_000, 1 << 30), caps)
}

fn lease_row_key() -> String {
    lease_key(LeaseId::new(5, 0))
}

/// A store holding operation 4, leased to `node-a` under lease `(5, 0)`.
fn leased() -> (Arc<Racing>, Durable) {
    let store = Racing::new();
    let durable = durable_over(&store);
    let queued = durable.submitted(OperationId(4), &request(1), &subject());
    queued.expect("submit");
    grant(&durable, 4, 0, "node-a").expect("a grant");
    (store, durable)
}

fn nobody() -> BTreeSet<WorkerId> {
    BTreeSet::new()
}

fn is_unreachable<T>(result: &Result<T, StateError>) -> bool {
    matches!(result, Err(StateError::Store(_)))
}

fn is_corrupt<T>(result: &Result<T, StateError>) -> bool {
    matches!(result, Err(StateError::Corrupt { .. }))
}

/// Asserts that a lease whose node has no session row is requeued by the second look that
/// finds it so, [`WAIT`] after the first on the store's clock and not before: the lease and
/// its index row are gone, the operation is queued, and the loss is counted once. Nothing is
/// left to sweep afterwards.
#[test]
fn a_lease_without_a_session_is_requeued_at_the_second_look_after_the_wait() {
    let (store, durable) = leased();
    let mut seen = BTreeMap::new();

    durable.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    assert_eq!(seen.len(), 1, "the first look remembers the lease");
    durable.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    store.advance(WAIT - Duration::from_secs(1));
    durable.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    assert_eq!((rows(&store, "lease/"), rows(&store, "queue/")), (1, 0));

    store.advance(Duration::from_secs(1));
    durable.sweep(WAIT, &nobody(), &mut seen).expect("a look");

    assert_eq!((rows(&store, "lease/"), rows(&store, "queue/")), (0, 1));
    assert_eq!(rows(&store, "lease-by-op/"), 0);
    assert_eq!(
        value_of(&store, "loss/0000000000000004"),
        1u64.to_be_bytes()
    );
    assert!(seen.is_empty(), "a requeued lease is remembered");
    durable.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    assert_eq!(rows(&store, "queue/"), 1);
}

/// Asserts that a wait of nothing still takes two looks: the first only remembers the
/// lease.
#[test]
fn the_first_look_at_a_lease_never_requeues_it() {
    let (store, durable) = leased();
    let mut seen = BTreeMap::new();

    durable
        .sweep(Duration::ZERO, &nobody(), &mut seen)
        .expect("a look");
    assert_eq!((rows(&store, "lease/"), rows(&store, "queue/")), (1, 0));

    durable
        .sweep(Duration::ZERO, &nobody(), &mut seen)
        .expect("a look");
    assert_eq!((rows(&store, "lease/"), rows(&store, "queue/")), (0, 1));
}

/// Asserts that a lease whose node has a session row, or is connected to the frontend that
/// looks, is left alone whatever the store's clock says, and that the lease of a node whose
/// session expires is remembered from the look that finds it so.
#[test]
fn a_lease_whose_node_is_heard_is_left_alone() {
    let (store, durable) = leased();
    durable
        .session(&node(), &DaemonInstance::new("daemon"), false, &reported())
        .expect("a session");
    let mut seen = BTreeMap::new();

    store.advance(WAIT / 2);
    durable.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    store.advance(WAIT / 2 - Duration::from_secs(1));
    durable.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    assert!(seen.is_empty(), "a node with a session was remembered");
    assert_eq!((rows(&store, "lease/"), rows(&store, "queue/")), (1, 0));

    store.advance(LEASE_GRACE);
    let connected = BTreeSet::from([node()]);
    durable.sweep(WAIT, &connected, &mut seen).expect("a look");
    durable.sweep(WAIT, &connected, &mut seen).expect("a look");
    assert!(seen.is_empty(), "a connected node was remembered");
    assert_eq!(rows(&store, "session/"), 0, "the session expired");
    assert_eq!((rows(&store, "lease/"), rows(&store, "queue/")), (1, 0));

    durable.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    assert_eq!(seen.len(), 1);
}

/// Asserts that a lease remembered as an orphan is forgotten when its node is heard again.
#[test]
fn a_lease_whose_node_comes_back_is_forgotten() {
    let (store, durable) = leased();
    let mut seen = BTreeMap::new();
    durable.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    assert_eq!(seen.len(), 1);

    durable
        .session(&node(), &DaemonInstance::new("daemon"), false, &reported())
        .expect("a session");
    store.advance(WAIT);
    durable.sweep(WAIT, &nobody(), &mut seen).expect("a look");

    assert!(seen.is_empty());
    assert_eq!((rows(&store, "lease/"), rows(&store, "queue/")), (1, 0));
}

/// Asserts that the wait starts over when the lease row is another write than the one first
/// seen: the lease is not requeued by a look made a wait after the first.
#[test]
fn a_lease_that_changed_starts_the_wait_over() {
    let (store, durable) = leased();
    let mut seen = BTreeMap::new();
    durable.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    let same = value_of(&store, &lease_row_key());
    overwrite(&store, &lease_row_key(), same);

    store.advance(WAIT);
    durable.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    assert_eq!((rows(&store, "lease/"), rows(&store, "queue/")), (1, 0));

    store.advance(WAIT);
    durable.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    assert_eq!((rows(&store, "lease/"), rows(&store, "queue/")), (0, 1));
}

/// Asserts that the wait starts over when a daemon has said `Hello` to a frontend since the
/// first look (the node's owner row is another write), and that the requeue is then made
/// with that owner row checked.
#[test]
fn a_node_claimed_since_the_first_look_starts_the_wait_over() {
    let (store, durable) = leased();
    let mut seen = BTreeMap::new();
    durable.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    let other: Arc<dyn Store> = store.clone();
    let (claimer, _) = Durable::open(other, EpochPolicy::Keep, 9).expect("open");
    claimer.arrive(&node(), at(0)).expect("claimed");

    store.advance(WAIT);
    durable.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    assert_eq!((rows(&store, "lease/"), rows(&store, "queue/")), (1, 0));
    store.advance(WAIT);
    durable.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    assert_eq!((rows(&store, "lease/"), rows(&store, "queue/")), (0, 1));
}

/// Asserts that a second look, whose requeue meets a write of another frontend of the
/// bytes `value` makes of the row `key` between the look and the transaction, reports
/// nothing, leaves the lease and its index row where they are, counts no loss, and forgets
/// the lease, so the wait starts over.
fn assert_requeue_loses_to_a_write(key: &str, value: impl FnOnce(&Racing) -> Vec<u8>) {
    let (store, durable) = leased();
    let mut seen = BTreeMap::new();
    durable.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    store.advance(WAIT);
    let landing = Txn::new().put(key, value(store.as_ref()), Cond::Any);
    store.land(key, landing);

    durable.sweep(WAIT, &nobody(), &mut seen).expect("a look");

    assert!(seen.is_empty(), "the lease was remembered");
    assert_eq!(rows(&store, "lease/"), 1);
    assert_eq!(rows(&store, "lease-by-op/"), 1);
    assert_eq!(rows(&store, "loss/"), 0);
}

/// Asserts that a requeue loses to a write of another frontend to the lease row between the
/// look and the transaction: nothing is written, and the sweep reports no error.
#[test]
fn a_requeue_that_loses_the_lease_row_to_another_write_changes_nothing() {
    let same = |store: &Racing| value_of(store, &lease_row_key());
    assert_requeue_loses_to_a_write(&lease_row_key(), same);
}

/// Asserts that a requeue loses to a write of another frontend to the operation's row (the
/// operation changed since the look): nothing is written.
#[test]
fn a_requeue_that_loses_the_operation_row_to_another_write_changes_nothing() {
    let same = |store: &Racing| value_of(store, "op/0000000000000004");
    assert_requeue_loses_to_a_write("op/0000000000000004", same);
}

/// Asserts that a requeue loses to the first claim of the node by a frontend, which writes
/// the node's owner row where the look found none: nothing is written.
#[test]
fn a_requeue_that_loses_the_owner_row_to_a_claim_changes_nothing() {
    assert_requeue_loses_to_a_write("owner/node-a", |_| pair_row(2, 1).to_vec());
}

/// Asserts that a requeue loses to a write of another frontend to the operation's queue
/// row, which the requeue writes where there is none: nothing is written.
#[test]
fn a_requeue_that_loses_the_queue_row_to_another_write_changes_nothing() {
    assert_requeue_loses_to_a_write("queue/0000000000000004", |_| Vec::new());
}

/// Asserts that a lease whose requeue lost to another write does not stop the look: the
/// leases after it are requeued by the same look.
#[test]
fn a_lost_requeue_does_not_stop_the_look_at_the_other_leases() {
    let (store, durable) = leased();
    let queued = durable.submitted(OperationId(5), &request(2), &subject());
    queued.expect("submit");
    grant(&durable, 5, 1, "node-b").expect("a grant");
    let mut seen = BTreeMap::new();
    durable.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    assert_eq!(seen.len(), 2);
    store.advance(WAIT);
    let same = value_of(&store, &lease_row_key());
    store.land(
        &lease_row_key(),
        Txn::new().put(lease_row_key(), same, Cond::Any),
    );

    durable.sweep(WAIT, &nobody(), &mut seen).expect("a look");

    assert_eq!((rows(&store, "lease/"), rows(&store, "queue/")), (1, 1));
    assert!(seen.is_empty(), "a lease was remembered");
}

/// Asserts that a requeue the store cannot take is reported and leaves the lease
/// remembered, so the next look that reaches the store requeues it at once.
#[test]
fn a_requeue_the_store_cannot_take_is_tried_again_by_the_next_look() {
    let (store, durable) = leased();
    let mut seen = BTreeMap::new();
    durable.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    store.advance(WAIT);

    store.set_down(true);
    let failed = durable.sweep(WAIT, &nobody(), &mut seen);
    assert!(is_unreachable(&failed), "{failed:?}");
    assert_eq!(seen.len(), 1, "the lease was forgotten");
    assert_eq!((rows(&store, "lease/"), rows(&store, "queue/")), (1, 0));

    store.set_down(false);
    durable.sweep(WAIT, &nobody(), &mut seen).expect("a look");
    assert_eq!((rows(&store, "lease/"), rows(&store, "queue/")), (0, 1));
}

/// Asserts that the loss the sweep counts is added to the count the store holds, and that a
/// lease whose operation has no row any more (it finished) puts nothing in the queue.
#[test]
fn the_loss_is_added_to_the_count_and_a_finished_operation_is_not_queued() {
    let (store, durable) = leased();
    overwrite(&store, "loss/0000000000000004", 2u64.to_be_bytes().to_vec());
    let mut seen = BTreeMap::new();
    durable
        .sweep(Duration::ZERO, &nobody(), &mut seen)
        .expect("a look");
    durable
        .sweep(Duration::ZERO, &nobody(), &mut seen)
        .expect("a look");
    assert_eq!(
        value_of(&store, "loss/0000000000000004"),
        3u64.to_be_bytes()
    );

    let (store, durable) = leased();
    remove(&store, "op/0000000000000004");
    let mut seen = BTreeMap::new();
    for _ in 0..2 {
        durable
            .sweep(Duration::ZERO, &nobody(), &mut seen)
            .expect("a look");
    }
    assert_eq!((rows(&store, "lease/"), rows(&store, "queue/")), (1, 0));
    assert_eq!(rows(&store, "loss/"), 0);
}

/// Asserts that a lease row the scheduler did not write fails the look and changes nothing.
#[test]
fn a_lease_row_that_cannot_be_read_fails_the_look() {
    let (store, durable) = leased();
    overwrite(&store, &lease_row_key(), vec![0xff]);

    let failed = durable.sweep(WAIT, &nobody(), &mut BTreeMap::new());

    assert!(is_corrupt(&failed), "{failed:?}");
    assert_eq!(rows(&store, "queue/"), 0);
}
