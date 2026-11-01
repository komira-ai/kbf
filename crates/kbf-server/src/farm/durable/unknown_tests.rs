//! Rows a later binary wrote, read back as a start reads them: a row that holds a command
//! this binary does not know is skipped, and the rows it can read load.

use kbf_state::fake::{Fake, Flaw};
use kbf_types::{ActionKey, DigestFunction, LeaseKind, Qos, Resources};

use super::*;
use crate::test_log::capture;

const OP_KNOWN: &str = "op/0000000000000001";
const OP_UNKNOWN: &str = "op/0000000000000002";
const LEASE_KNOWN: &str = "lease/0000000000000005.0000000000000000";
const LEASE_UNKNOWN: &str = "lease/0000000000000005.0000000000000001";

fn store() -> Arc<Fake> {
    let clock = Arc::new(kbf_state::ManualClock::default());
    Arc::new(Fake::with_flaw(clock, Flaw::None))
}

fn open(store: &Arc<Fake>) -> Durable {
    let store: Arc<dyn Store> = store.clone();
    Durable::open(store, EpochPolicy::Keep, 100)
        .expect("open")
        .0
}

fn put(store: &Fake, key: &str, value: Vec<u8>) {
    let put = Txn::new().put(key, value, Cond::Any);
    store.txn(put).expect("store").expect("put");
}

/// A metadata command whose `oneof` arm (field 21) no message of this build names.
fn unknown() -> Vec<u8> {
    vec![0x0a, 0x03, 0xaa, 0x01, 0x00]
}

fn subject() -> Subject {
    Subject::of(Described::default()).0
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

fn grant() -> LeaseGrant {
    LeaseGrant {
        lease: LeaseId::new(5, 0),
        operation: OperationId(1),
        worker: WorkerId::new("node-a"),
        cost: Default::default(),
    }
}

/// Asserts that a start skips an `op/` row and a `lease/` row that hold a command this
/// binary does not know, and loads the operation and the lease whose rows it can read.
#[test]
fn unknown_rows_are_skipped_and_the_known_rows_load() {
    let store = store();
    let durable = open(&store);
    let submit = encode(FarmInput::Submit(request(1))).expect("encode");
    let record = FarmInput::Committed(ControlRecord::Lease(grant()));
    let lease = encode(record).expect("encode");
    put(&store, OP_KNOWN, submit);
    put(&store, OP_UNKNOWN, unknown());
    put(&store, LEASE_KNOWN, lease);
    put(&store, LEASE_UNKNOWN, unknown());
    let stored = durable.load().expect("the rows load");
    let loaded: Vec<_> = stored
        .operations
        .iter()
        .map(|op| (op.operation, op.lease.clone()))
        .collect();
    assert_eq!(loaded, [(OperationId(1), Some(grant()))]);
}

/// Asserts that a single `op/` row a write depends on is refused, naming the row, when it
/// holds a command this binary does not know: only a scan skips such a row.
#[test]
fn an_unknown_row_a_write_depends_on_is_refused_naming_it() {
    let store = store();
    let durable = open(&store);
    put(&store, OP_UNKNOWN, unknown());
    let Err(StateError::Corrupt { key, why }) = durable.unpin(Txn::new(), OperationId(2)) else {
        panic!("the row was not refused");
    };
    assert_eq!(key, OP_UNKNOWN);
    assert_eq!(why, "a command this binary does not know");
}

/// Asserts that a request for an action another submit queued joins that operation, and
/// carries its lease, when the store also holds a `lease/` row that holds a command this
/// binary does not know: the row is passed over, not refused, and not logged.
#[test]
fn a_request_joins_an_operation_past_an_unknown_lease_row() {
    let store = store();
    let durable = open(&store);
    let queued = durable.submit(OperationId(1), &request(1), &subject());
    assert!(
        matches!(queued, Ok(Submitted::Queued(OperationId(1)))),
        "{queued:?}"
    );
    // A granted operation has left the queue: the lease is looked up only then.
    store
        .txn(Txn::new().delete(queue_key(OperationId(1)), Cond::Any))
        .expect("the store answers")
        .expect("the queue row goes");
    let record = FarmInput::Committed(ControlRecord::Lease(grant()));
    put(&store, LEASE_KNOWN, encode(record).expect("encode"));
    put(&store, LEASE_UNKNOWN, unknown());
    let lines = capture();
    let joined = durable.submit(OperationId(2), &request(1), &subject());
    let Ok(Submitted::Joined(op)) = joined else {
        panic!("the request did not join");
    };
    assert_eq!(op.operation, OperationId(1));
    assert_eq!(op.lease, Some(grant()));
    let text = lines.text();
    assert!(!text.contains(LEASE_UNKNOWN), "{text}");
    // Positive control: a start, over the same rows, logs the row in this same capture, so
    // the absence above is not an empty capture.
    durable.load().expect("the rows load");
    let text = lines.text();
    assert!(text.contains(LEASE_UNKNOWN), "{text}");
}

/// Asserts that a start logs the key of a row it skips, and that the search for the
/// operations a node holds leases for, which a worker's arrival makes, passes over the same
/// row without logging it.
#[test]
fn only_a_start_logs_the_rows_it_skips() {
    let store = store();
    let durable = open(&store);
    put(&store, LEASE_UNKNOWN, unknown());
    let lines = capture();
    durable
        .held_by(&grant().worker)
        .expect("the search reads the rows");
    let text = lines.text();
    assert!(!text.contains(LEASE_UNKNOWN), "{text}");
    durable.load().expect("the rows load");
    let text = lines.text();
    assert!(text.contains(LEASE_UNKNOWN), "{text}");
}
