//! The ownership of a daemon's node when several frontends share one state store: the
//! `Hello` that makes a frontend the owner, the writes of a node's leases that only the
//! owner may make, and the look at the owner row that a heartbeat's acknowledgement waits
//! on. Each test stands two handles over one store for two frontends.

use kbf_caps::NodeCaps;
use kbf_types::Resources;

use super::blocks_tests::{Racing, request};
use super::owner::{Owner, pair_row};
use super::transition_tests::{answer, ending, requeue, rows, start, subject};
use super::*;

const OWNER_ROW: &str = "owner/node-a";
const SESSION_ROW: &str = "session/node-a";

fn node() -> WorkerId {
    WorkerId::new("node-a")
}

fn at(secs: u64) -> FarmTime {
    FarmTime::from_millis(secs * 1_000)
}

fn named(frontend: u64, counter: u64) -> Owner {
    Owner { frontend, counter }
}

/// A handle over `store` for the frontend numbered `id`.
fn frontend(store: &Arc<Racing>, id: u64) -> Durable {
    let store: Arc<dyn Store> = store.clone();
    let (durable, _) = Durable::open(store, EpochPolicy::Keep, id).expect("open");
    durable
}

pub(super) fn value_of(store: &Racing, key: &str) -> Vec<u8> {
    let row = store.get(key).expect("read");
    row.expect("a row").value
}

/// Replaces the row at `key`, whatever it holds.
pub(super) fn overwrite(store: &Racing, key: &str, value: Vec<u8>) {
    let put = Txn::new().put(key, value, Cond::Any);
    store.txn(put).expect("store").expect("put");
}

/// Deletes the row at `key`.
pub(super) fn remove(store: &Racing, key: &str) {
    let gone = Txn::new().delete(key, Cond::Any);
    store.txn(gone).expect("store").expect("deleted");
}

/// Queues operation `operation` through `durable`.
fn queue(durable: &Durable, operation: u64) {
    let queued = durable.submitted(OperationId(operation), &request(1), &subject());
    queued.expect("submit");
}

pub(super) fn grant(
    durable: &Durable,
    operation: u64,
    seq: u64,
    worker: &str,
) -> Result<(), StateError> {
    durable.persist(&[Effect::Start(start(operation, seq, worker))], None)
}

fn finish(durable: &Durable, operation: u64, seq: u64) -> Result<(), StateError> {
    let ended = ending("finished");
    durable.persist(&[Effect::Answer(answer(operation, seq))], Some(&ended))
}

fn give_up(durable: &Durable, operation: u64, seq: u64) -> Result<(), StateError> {
    let given_up = requeue(operation, seq);
    durable.requeued(&given_up, None, None, &FloorChange::default())
}

fn is_stale<T>(result: &Result<T, StateError>) -> bool {
    matches!(result, Err(StateError::StaleOwner(_)))
}

fn is_conflict<T>(result: &Result<T, StateError>) -> bool {
    matches!(result, Err(StateError::Conflict(_)))
}

fn is_corrupt<T>(result: &Result<T, StateError>) -> bool {
    matches!(result, Err(StateError::Corrupt { .. }))
}

fn reported() -> Reported {
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")]).expect("caps");
    Reported::serving(Resources::new(2_000, 1 << 30), caps)
}

/// Asserts that a daemon's `Hello` makes the frontend the owner of the node at the next
/// counter, and says who owned it before. The row is the frontend's id and the counter,
/// eight bytes each, big-endian, pinned byte for byte because another release reads it.
#[test]
fn each_hello_makes_the_frontend_the_owner_at_the_next_counter() {
    let store = Racing::new();
    let (first, second) = (frontend(&store, 7), frontend(&store, 9));

    let arrival = first.arrive(&node(), at(0)).expect("a first Hello");
    assert_eq!((arrival.previous, arrival.frontend), (None, 7));
    let row = value_of(&store, OWNER_ROW);
    assert_eq!(row, [0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 1]);

    let arrival = second.arrive(&node(), at(1)).expect("a second Hello");
    assert_eq!(arrival.previous, Some(named(7, 1)));
    let row = value_of(&store, OWNER_ROW);
    assert_eq!(row, [0, 0, 0, 0, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0, 2]);

    let arrival = second.arrive(&node(), at(2)).expect("a third Hello");
    assert_eq!(arrival.previous, Some(named(9, 2)));
}

/// Asserts that a grant for a node another frontend owns now is refused as stale and writes
/// nothing; that the node's later writes are refused too, whatever their rows say; that a
/// node the frontend never claimed is granted as before; and that the owner's grant stands.
#[test]
fn a_frontend_a_daemon_has_left_grants_its_node_nothing() {
    let store = Racing::new();
    let (left, taker) = (frontend(&store, 1), frontend(&store, 2));
    left.arrive(&node(), at(0)).expect("a Hello");
    taker.arrive(&node(), at(1)).expect("moved");
    queue(&left, 4);
    queue(&left, 5);
    let txns = store.commit_stats().txns;

    let refused = grant(&left, 4, 0, "node-a");

    assert!(is_stale(&refused), "{refused:?}");
    assert_eq!(store.commit_stats().txns, txns, "a stale grant wrote");
    assert_eq!((rows(&store, "queue/"), rows(&store, "lease/")), (2, 0));
    assert_eq!(rows(&store, "lease-by-op/"), 0);
    let again = grant(&left, 4, 1, "node-a");
    assert!(is_stale(&again), "{again:?}");

    grant(&left, 5, 2, "node-b").expect("a node never claimed");
    grant(&taker, 4, 3, "node-a").expect("the owner's grant");
    assert_eq!((rows(&store, "queue/"), rows(&store, "lease/")), (0, 2));
    assert_eq!(rows(&store, "lease-by-op/"), 2);
}

/// Asserts that a lease given up, and a result taken, by the frontend a daemon has left are
/// refused as stale and write nothing, and that the frontend the daemon moved to takes the
/// result as it would have.
#[test]
fn a_lease_is_given_up_and_finished_only_by_the_owner_of_its_node() {
    let store = Racing::new();
    let (left, taker) = (frontend(&store, 1), frontend(&store, 2));
    left.arrive(&node(), at(0)).expect("a Hello");
    queue(&left, 4);
    grant(&left, 4, 0, "node-a").expect("a grant");
    taker.arrive(&node(), at(1)).expect("moved");
    let txns = store.commit_stats().txns;

    let given_up = give_up(&left, 4, 0);
    let finished = finish(&left, 4, 0);

    assert!(is_stale(&given_up), "{given_up:?}");
    assert!(is_stale(&finished), "{finished:?}");
    assert_eq!(store.commit_stats().txns, txns, "a stale write wrote");
    assert_eq!((rows(&store, "lease/"), rows(&store, "done/")), (1, 0));
    assert_eq!(rows(&store, "lease-by-op/"), 1);

    finish(&taker, 4, 0).expect("the owner's result");
    assert_eq!((rows(&store, "lease/"), rows(&store, "done/")), (0, 1));
    assert_eq!(rows(&store, "lease-by-op/"), 0);
}

/// Asserts that the owner of a node gives its lease up as it did: the operation is queued
/// again, and the lease and its index row are gone.
#[test]
fn the_owner_of_a_node_gives_its_lease_up() {
    let store = Racing::new();
    let first = frontend(&store, 1);
    first.arrive(&node(), at(0)).expect("a Hello");
    queue(&first, 4);
    grant(&first, 4, 0, "node-a").expect("a grant");

    give_up(&first, 4, 0).expect("a give-up");

    assert_eq!((rows(&store, "queue/"), rows(&store, "lease/")), (1, 0));
    assert_eq!(rows(&store, "lease-by-op/"), 0);
}

/// Asserts that a frontend whose node's owner row is gone writes nothing for the node: it
/// cannot show that it still owns it.
#[test]
fn a_frontend_whose_owner_row_is_gone_writes_nothing_for_the_node() {
    let store = Racing::new();
    let first = frontend(&store, 1);
    first.arrive(&node(), at(0)).expect("a Hello");
    queue(&first, 4);
    remove(&store, OWNER_ROW);

    let refused = grant(&first, 4, 0, "node-a");

    assert!(is_stale(&refused), "{refused:?}");
    assert_eq!(rows(&store, "lease/"), 0);
}

/// Asserts that a grant that read the owner row as its own, and finds it changed when it
/// commits because the daemon said `Hello` to another frontend in between, writes nothing and
/// fails as stale.
#[test]
fn a_node_that_changes_hands_between_the_read_and_the_commit_is_stale() {
    let store = Racing::new();
    let first = frontend(&store, 1);
    first.arrive(&node(), at(0)).expect("a Hello");
    queue(&first, 4);
    let moved = Txn::new().put(OWNER_ROW, pair_row(2, 2), Cond::Any);
    store.land(OWNER_ROW, moved);

    let refused = grant(&first, 4, 0, "node-a");

    assert!(is_stale(&refused), "{refused:?}");
    assert_eq!((rows(&store, "queue/"), rows(&store, "lease/")), (1, 0));
    let later = grant(&first, 4, 1, "node-a");
    assert!(is_stale(&later), "{later:?}");
}

/// Asserts that a conflict on a row other than the owner row stays a conflict, and leaves
/// the node the frontend's: its owner row still stands.
#[test]
fn a_conflict_on_another_row_is_not_a_change_of_owner() {
    let store = Racing::new();
    let first = frontend(&store, 1);
    first.arrive(&node(), at(0)).expect("a Hello");
    queue(&first, 4);
    let taken = Txn::new().delete(queue_key(OperationId(4)), Cond::Any);
    store.land(&queue_key(OperationId(4)), taken);

    let refused = grant(&first, 4, 0, "node-a");

    assert!(is_conflict(&refused), "{refused:?}");
    assert!(first.owns(&node(), at(100)).expect("a look"));
}

/// Asserts that a frontend that loses every try to make itself a node's owner, to others
/// who write the row first, gives the node up: the error is the conflict, and nothing is
/// written for the node afterwards.
#[test]
fn a_frontend_that_loses_every_try_for_a_node_gives_it_up() {
    let store = Racing::new();
    let first = frontend(&store, 1);
    queue(&first, 4);
    let tries = u64::try_from(super::blocks::CAS_TRIES).expect("a small number");
    for counter in 1..=tries {
        let other = Txn::new().put(OWNER_ROW, pair_row(2, counter), Cond::Any);
        store.land(OWNER_ROW, other);
    }

    let lost = first.arrive(&node(), at(0));

    assert!(is_conflict(&lost), "{lost:?}");
    let refused = grant(&first, 4, 0, "node-a");
    assert!(is_stale(&refused), "{refused:?}");
}

/// Asserts that the owner row is read again when the frontend's last look is the cache TTL
/// old and not before, that a look that finds the row its own extends the cache, that one
/// that does not finds the node lost for good, and that a node never claimed is nobody's.
#[test]
fn the_owner_row_is_read_again_when_the_last_look_is_a_cache_ttl_old() {
    let store = Racing::new();
    let first = frontend(&store, 1);
    first.arrive(&node(), at(10)).expect("a Hello");
    let just_before = at(10).saturating_add(OWNER_CACHE_TTL - Duration::from_millis(1));
    let due = at(10).saturating_add(OWNER_CACHE_TTL);
    remove(&store, OWNER_ROW);

    assert!(first.owns(&node(), just_before).expect("cached"));
    assert!(!first.owns(&node(), due).expect("read"));
    assert!(!first.owns(&node(), due).expect("lost"));

    let store = Racing::new();
    let second = frontend(&store, 1);
    second.arrive(&node(), at(10)).expect("a Hello");
    assert!(second.owns(&node(), due).expect("read"));
    remove(&store, OWNER_ROW);
    let later = due.saturating_add(OWNER_CACHE_TTL - Duration::from_millis(1));
    assert!(second.owns(&node(), later).expect("cached"));
    let after = due.saturating_add(OWNER_CACHE_TTL);
    assert!(!second.owns(&node(), after).expect("read"));

    let stranger = WorkerId::new("node-z");
    assert!(!second.owns(&stranger, at(0)).expect("never claimed"));
}

/// Asserts that a node whose owner row names another frontend is not owned, and that a
/// write for it is refused.
#[test]
fn a_node_another_frontend_claimed_is_not_owned() {
    let store = Racing::new();
    let (first, second) = (frontend(&store, 1), frontend(&store, 2));
    first.arrive(&node(), at(0)).expect("a Hello");
    second.arrive(&node(), at(1)).expect("moved");

    assert!(!first.owns(&node(), at(60)).expect("read"));
    assert!(second.owns(&node(), at(60)).expect("read"));
    queue(&first, 4);
    let refused = grant(&first, 4, 0, "node-a");
    assert!(is_stale(&refused), "{refused:?}");
}

/// Asserts that a `Hello` reads what it takes over: the owner before, the unfinished
/// operations whose leases name the node (not another node's, and not a lease whose
/// operation has no row), and the daemon process the node's session row names (none when
/// the row is missing, or is not a registration).
#[test]
fn a_hello_reads_the_leases_and_the_session_of_the_node_it_takes_over() {
    let store = Racing::new();
    let (left, taker) = (frontend(&store, 1), frontend(&store, 2));
    let first = left.arrive(&node(), at(0)).expect("a Hello");
    assert_eq!(first.session, None);
    queue(&left, 4);
    queue(&left, 5);
    grant(&left, 4, 0, "node-a").expect("a grant");
    grant(&left, 5, 1, "node-b").expect("a grant on another node");
    let orphan = LeaseGrant {
        lease: LeaseId::new(5, 9),
        operation: OperationId(9),
        worker: node(),
        cost: Default::default(),
    };
    let record = ControlRecord::Lease(orphan.clone());
    let row = encode(FarmInput::Committed(record)).expect("row");
    overwrite(&store, &lease_key(orphan.lease), row);
    let daemon = DaemonInstance::new("daemon-1");
    let registered = left.session(&node(), &daemon, false, &reported());
    registered.expect("a session");

    let arrival = taker.arrive(&node(), at(1)).expect("moved");

    assert_eq!(arrival.previous, Some(named(1, 1)));
    assert_eq!(arrival.session, Some(daemon));
    let held: Vec<OperationId> = arrival.leases.iter().map(|op| op.operation).collect();
    assert_eq!(held, [OperationId(4)]);
    let held_op = &arrival.leases[0];
    let lease = held_op.lease.as_ref().map(|grant| grant.lease);
    assert_eq!(lease, Some(LeaseId::new(5, 0)));

    overwrite(&store, SESSION_ROW, vec![0xff]);
    let unreadable = taker.arrive(&node(), at(2)).expect("a Hello");
    assert_eq!(unreadable.session, None);
    let other = encode(FarmInput::Submit(request(1))).expect("row");
    overwrite(&store, SESSION_ROW, other);
    let not_a_session = taker.arrive(&node(), at(3)).expect("a Hello");
    assert_eq!(not_a_session.session, None);
}

/// Asserts that an owner row that is not one is refused naming the row: at a `Hello`, at a
/// write for a node the frontend claimed before the row was damaged, and at a look; and that
/// a node whose claim failed on it is lost, so that a write for it is refused as stale.
#[test]
fn an_owner_row_that_is_not_one_is_refused_naming_it() {
    let store = Racing::new();
    let first = frontend(&store, 1);
    overwrite(&store, OWNER_ROW, vec![1, 2, 3]);
    let error = first.arrive(&node(), at(0)).expect_err("a damaged row");
    let said = error.to_string();
    assert!(said.contains(OWNER_ROW), "{said}");
    assert!(said.contains("not a 16-byte owner"), "{said}");
    queue(&first, 4);
    let unclaimed = grant(&first, 4, 0, "node-a");
    assert!(is_stale(&unclaimed), "{unclaimed:?}");

    let store = Racing::new();
    let second = frontend(&store, 1);
    second.arrive(&node(), at(0)).expect("a Hello");
    queue(&second, 4);
    overwrite(&store, OWNER_ROW, vec![1, 2, 3]);
    let refused = grant(&second, 4, 0, "node-a");
    assert!(is_corrupt(&refused), "{refused:?}");
    let look = second.owns(&node(), at(100));
    assert!(is_corrupt(&look), "{look:?}");
}

/// Asserts that a renewal puts the registration this handle wrote last again when the row
/// is gone (it expired, or the write was refused), as a session row that expires.
#[test]
fn a_renewal_puts_the_registration_again_when_its_row_is_gone() {
    let store = Racing::new();
    let first = frontend(&store, 1);
    let daemon = DaemonInstance::new("daemon-1");
    let registered = first.session(&node(), &daemon, false, &reported());
    registered.expect("a session");
    let written = value_of(&store, SESSION_ROW);
    remove(&store, SESSION_ROW);

    first.renew(&node()).expect("put again");

    assert_eq!(value_of(&store, SESSION_ROW), written);
    let row = store.get(SESSION_ROW).expect("read");
    assert!(row.expect("a row").expires_at.is_some());
}

/// Asserts that a store holding `owner/` and `lease-by-op/` rows reads as it did without
/// them for a release that does not know them: every prefix it scans meets neither, and
/// neither begins with it.
#[test]
fn a_store_with_owner_and_index_rows_reads_as_it_did_without_them() {
    let store = Racing::new();
    let first = frontend(&store, 1);
    first.arrive(&node(), at(0)).expect("a Hello");
    queue(&first, 4);
    grant(&first, 4, 0, "node-a").expect("a grant");
    assert_eq!(rows(&store, "owner/"), 1);
    assert_eq!(rows(&store, "lease-by-op/"), 1);

    let known = [
        "op/",
        "queue/",
        "lease/",
        "session/",
        "loss/",
        "fate/",
        "floor/",
        "subject/",
        "done/",
        "cordon/",
        "inflight/",
        "opkey/",
        "opwall/",
        "keyfloor/",
        "est/",
        "meta/",
    ];
    for prefix in known {
        let scanned = store.scan_prefix(prefix).expect("scan");
        for new in ["owner/", "lease-by-op/"] {
            let meets = scanned.iter().any(|(key, _)| key.starts_with(new));
            assert!(!meets, "a scan of {prefix} meets a {new} row");
        }
    }
}

/// Asserts that a lease given up by the frontend a daemon has left, which is refused as
/// stale, still writes the floor the kill raised: a floor is the farm's, and the refused
/// give-up is not tried again. The lease and the queue stay as the owner has them.
#[test]
fn a_give_up_refused_for_the_owner_still_writes_the_floors() {
    let store = Racing::new();
    let (left, taker) = (frontend(&store, 1), frontend(&store, 2));
    left.arrive(&node(), at(0)).expect("a Hello");
    queue(&left, 4);
    grant(&left, 4, 0, "node-a").expect("a grant");
    taker.arrive(&node(), at(1)).expect("moved");
    let floor = Floor {
        key: request(1).key,
        ask: 2 << 30,
        stamp: 1,
    };
    let floors = FloorChange {
        raised: Some(floor),
        ..FloorChange::default()
    };

    let refused = left.requeued(&requeue(4, 0), None, None, &floors);

    assert!(is_stale(&refused), "{refused:?}");
    assert_eq!(rows(&store, "floor/"), 1);
    assert_eq!((rows(&store, "queue/"), rows(&store, "lease/")), (0, 1));
}

/// Asserts that a claim whose arrival cannot be read leaves the node lost to the frontend (a
/// write for it is refused as stale), and that the claim made again, which finds the
/// frontend's own row, reports the owner the first claim replaced and not the frontend
/// itself, and the node is owned again.
#[test]
fn a_claim_made_again_after_an_unread_arrival_reports_the_owner_it_replaced() {
    let store = Racing::new();
    let (left, taker) = (frontend(&store, 1), frontend(&store, 2));
    left.arrive(&node(), at(0)).expect("a Hello");
    let damaged = lease_key(LeaseId::new(5, 9));
    overwrite(&store, &damaged, vec![0xff]);

    let unread = taker.arrive(&node(), at(1));

    assert!(unread.is_err(), "{unread:?}");
    queue(&taker, 4);
    let refused = grant(&taker, 4, 0, "node-a");
    assert!(is_stale(&refused), "{refused:?}");

    remove(&store, &damaged);
    let again = taker.arrive(&node(), at(2)).expect("the claim again");
    assert_eq!(again.previous, Some(named(1, 1)));
    grant(&taker, 4, 1, "node-a").expect("owned again");
    let later = taker.arrive(&node(), at(3)).expect("a Hello");
    assert_eq!(later.previous, Some(named(2, 3)));
}

/// Asserts that the owner an unread claim replaced is not reported when the row names
/// another frontend by the time the claim is made again: that frontend is the one replaced.
#[test]
fn a_claim_made_again_over_another_frontends_row_reports_that_frontend() {
    let store = Racing::new();
    let (left, taker) = (frontend(&store, 1), frontend(&store, 2));
    let other = frontend(&store, 3);
    left.arrive(&node(), at(0)).expect("a Hello");
    let damaged = lease_key(LeaseId::new(5, 9));
    overwrite(&store, &damaged, vec![0xff]);
    let unread = taker.arrive(&node(), at(1));
    assert!(unread.is_err(), "{unread:?}");
    remove(&store, &damaged);
    other.arrive(&node(), at(2)).expect("moved");

    let again = taker.arrive(&node(), at(3)).expect("the claim again");

    assert_eq!(again.previous, Some(named(3, 3)));
}
