//! The load of the store with the checks `js-store` leaves to this server.

use std::sync::Arc;

use js_store::{Rows, StateError, keys};
use kbf_estimator::{Basis, Cfg, Instance, Key, PlatformClass, Shape};
use kbf_sched::{Floor, FloorChange, KeyFloor, Requeue, RequeueReason};
use kbf_state::fake::Fake;
use kbf_types::sched::usage::LeaseCost;
use kbf_types::{FarmTime, LeaseGrant, LeaseId, OperationId, WaiterId, WorkKey, WorkerId};

use super::ext::KbfExt;
use super::load::load;
use super::testing::{charged, parcel, request, rows, store};

fn summary() -> Key {
    Key::new(
        Instance::new("main"),
        PlatformClass::new("linux/x86_64"),
        Basis::Cgroup,
        Shape(0x0123_4567_89ab_cdef),
        Some(Cfg(7)),
    )
}

fn giveup() -> Requeue {
    Requeue {
        operation: OperationId(4),
        lease: LeaseId::new(5, 0),
        worker: WorkerId::new("node-a"),
        reason: RequeueReason::Silent,
    }
}

/// Writes the floors `changed` through `rows`, naming the size summary `name` for a key floor.
fn raise(rows: &Rows, changed: FloorChange, name: Option<&str>) {
    rows.requeued(&giveup(), None, None, &changed, name)
        .expect("a floor");
}

fn floor(key: WorkKey) -> FloorChange {
    FloorChange {
        raised: Some(Floor {
            key,
            ask: 16 << 30,
            stamp: 2,
        }),
        ..FloorChange::default()
    }
}

fn key_floor(key: u64) -> FloorChange {
    FloorChange {
        key_raised: Some(KeyFloor {
            key,
            ask: 16 << 30,
            stamp: 2,
        }),
        ..FloorChange::default()
    }
}

fn corrupt(error: StateError) -> (String, String) {
    match error {
        StateError::Corrupt { key, why } => (key, why),
        other => panic!("{other:?}"),
    }
}

/// Asserts that a floor row whose dedup text is not an action digest is loaded by `js-store`
/// and refused by this server's load, naming the row, and that a floor of an action loads.
///
/// Catches: a start that takes a floor for work that is not an action (the farm's reader
/// refuses such a row; `js-store` keeps any text), and a load that refuses a good floor.
#[test]
fn a_floor_row_whose_text_is_not_a_digest_is_refused_naming_it() {
    let store: Arc<Fake> = store();
    let rows = rows(&store, 1);
    raise(&rows, floor(request(1).key), None);
    let loaded = load(&rows).expect("a floor of an action loads");
    assert_eq!(loaded.stored.floors.len(), 1);

    let odd = WorkKey::new("main", "not a digest");
    raise(&rows, floor(odd.clone()), None);
    assert_eq!(rows.load().expect("js-store loads it").floors.len(), 2);
    let (key, why) = corrupt(load(&rows).expect_err("refused"));
    assert_eq!(key, keys::floor(&odd));
    assert_eq!(why, "not a digest");
}

/// Asserts that the size summary a key floor names is parsed, and that a name that does not
/// parse is dropped (the action starts cold) while the key floor stays.
///
/// Catches: a name handed on as it is (the learned sizes would look for a row that cannot
/// exist), and a key floor lost with its name.
#[test]
fn a_key_floors_summary_name_is_parsed_and_a_bad_one_is_dropped() {
    let store: Arc<Fake> = store();
    let rows = rows(&store, 1);
    raise(&rows, key_floor(7), Some(&KbfExt.summary_name(&summary())));
    raise(&rows, key_floor(8), Some("est/not-a-key"));
    raise(&rows, key_floor(9), None);
    let loaded = load(&rows).expect("loads");
    assert_eq!(loaded.summaries, vec![(7, summary())]);
    assert!(loaded.stored.key_names.is_empty());
    let keys: Vec<u64> = loaded.stored.key_floors.iter().map(|f| f.key).collect();
    assert_eq!(keys, vec![7, 8, 9]);
}

/// Asserts that a lease granted through `js-store` with a cost is recovered with that cost,
/// `retry` included, and that one granted with the default cost, which its row leaves out, is
/// recovered with none (the scheduler then charges it its request).
///
/// Catches: a recovered lease charged less than it was granted, or a rerun up the memory
/// ladder charged as a first run, after a restart; and a legacy lease charged as nothing.
#[test]
fn a_lease_is_recovered_with_the_cost_it_was_granted() {
    let store: Arc<Fake> = store();
    let rows = rows(&store, 1);
    let first = rows.reserve_operation(OperationId(4)).expect("ids");
    rows.reserve_seqs(0).expect("sequences");
    let node = WorkerId::new("node-a");
    rows.arrive(&node, FarmTime::from_millis(0))
        .expect("a Hello");
    for (i, tag, cost) in [(0, 1, charged()), (1, 2, LeaseCost::default())] {
        let operation = OperationId(first.0 + i);
        rows.submit(operation, &request(tag), &parcel(), None)
            .expect("submit");
        let grant = LeaseGrant {
            lease: LeaseId::new(5, i),
            operation,
            worker: node.clone(),
            cost,
        };
        rows.granted(&grant).expect("grant");
    }
    let loaded = load(&rows).expect("loads");
    let costs: Vec<_> = loaded
        .stored
        .operations
        .into_iter()
        .map(|op| op.recovered(WaiterId(1)).lease.expect("a lease").cost)
        .collect();
    assert_eq!(costs, vec![Some(charged()), None]);
    assert!(charged().retry);
}

/// Asserts that a load error of `js-store` passes through.
///
/// Catches: a load that swallows a store it cannot read.
#[test]
fn a_store_that_cannot_be_read_fails_the_load() {
    let store: Arc<Fake> = store();
    let rows = rows(&store, 1);
    super::testing::overwrite(&store, keys::NEXT_OP, vec![1]);
    let (key, _) = corrupt(load(&rows).expect_err("a bad counter"));
    assert_eq!(key, keys::NEXT_OP);
}

/// Asserts that an `op/` row that does not parse fails the load, naming the row, where the
/// same store loaded before the row was damaged.
///
/// Catches: a load that skips a row it cannot read, which would leave the operation with
/// no caller and its lease running to its timeout.
#[test]
fn a_garbage_op_row_fails_the_load_naming_the_row() {
    let store: Arc<Fake> = store();
    let rows = rows(&store, 1);
    let operation = rows.reserve_operation(OperationId(4)).expect("ids");
    rows.reserve_seqs(0).expect("sequences");
    rows.submit(operation, &request(1), &parcel(), None)
        .expect("submit");
    let loaded = load(&rows).expect("an intact store loads");
    assert_eq!(loaded.stored.operations.len(), 1);

    super::testing::overwrite(&store, &keys::operation(operation), vec![0xff, 0xff, 0xff]);
    let (key, _) = corrupt(load(&rows).expect_err("a garbage row"));
    assert_eq!(key, keys::operation(operation));
}
