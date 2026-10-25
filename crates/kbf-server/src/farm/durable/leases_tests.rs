//! Finding the lease of an operation: through its `lease-by-op/` row, without reading every
//! `lease/` row, and as before when the row is missing or does not stand (a lease granted
//! by a release that wrote none).

use kbf_types::Refusal;

use super::blocks_tests::{Racing, durable_over, farm_over, request};
use super::owner::pair_row;
use super::owner_tests::{grant, overwrite, remove, value_of};
use super::transition_tests::{ending, rows, subject};
use super::*;

const INDEX_4: &str = "lease-by-op/0000000000000004";

/// Submits `request(tag)` as operation `operation`, which joins an operation of that action
/// the store holds.
fn submit(durable: &Durable, operation: u64, tag: u8) -> Result<Submitted, StateError> {
    durable.submit(OperationId(operation), &request(tag), &subject())
}

/// The lease the operation a request joined holds; `None` for a request that did not join,
/// and for an operation that holds none.
fn lease_found(submitted: Result<Submitted, StateError>) -> Option<LeaseId> {
    match submitted.expect("accepted") {
        Submitted::Joined(op) => op.lease.map(|grant| grant.lease),
        Submitted::Queued(_) => None,
    }
}

/// Operation 4 of `request(1)`, running under lease `(5, 7)` on `node-a`.
fn running(durable: &Durable) {
    submit(durable, 4, 1).expect("queued");
    grant(durable, 4, 7, "node-a").expect("a grant");
}

/// Asserts that a grant puts the lease id under the key of its operation, eight bytes of
/// term and eight of sequence, big-endian, pinned byte for byte because another release
/// reads it.
#[test]
fn a_grant_puts_the_lease_id_under_the_key_of_the_operation() {
    let store = Racing::new();
    let durable = durable_over(&store);

    running(&durable);

    let row = value_of(&store, INDEX_4);
    assert_eq!(row, [0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 7]);
}

/// Asserts that a request that joins a running operation finds its lease through the index,
/// and one that joins a queued operation finds none, and that neither reads the `lease/` rows;
/// and that a request for an action nothing runs queues.
#[test]
fn a_joined_request_finds_its_lease_without_reading_the_lease_rows() {
    let store = Racing::new();
    let durable = durable_over(&store);
    running(&durable);
    submit(&durable, 5, 2).expect("queued");
    let scans = store.lease_scans();

    let on_a_node = lease_found(submit(&durable, 9, 1));
    let waiting = lease_found(submit(&durable, 9, 2));
    let fresh = lease_found(submit(&durable, 9, 3));

    assert_eq!(on_a_node, Some(LeaseId::new(5, 7)));
    assert_eq!((waiting, fresh), (None, None));
    assert_eq!(store.lease_scans(), scans, "the lease rows were read");
}

/// Asserts that a request that joins an operation whose lease has no index row (a release
/// that wrote none granted it) still finds the lease, by reading the `lease/` rows.
#[test]
fn a_lease_with_no_index_row_is_found_by_reading_the_lease_rows() {
    let store = Racing::new();
    let durable = durable_over(&store);
    running(&durable);
    remove(&store, INDEX_4);
    let scans = store.lease_scans();

    let found = lease_found(submit(&durable, 9, 1));

    assert_eq!(found, Some(LeaseId::new(5, 7)));
    assert_eq!(store.lease_scans(), scans + 1);
}

/// Asserts that an index row that names a lease that is not there (left by a release that
/// ended the lease and did not delete it) is not taken for the operation holding none: the
/// lookup reads the `lease/` rows.
#[test]
fn an_index_row_naming_a_lease_that_is_gone_is_not_believed() {
    let store = Racing::new();
    let durable = durable_over(&store);
    running(&durable);
    overwrite(&store, INDEX_4, pair_row(5, 99).to_vec());
    let scans = store.lease_scans();

    let found = lease_found(submit(&durable, 9, 1));

    assert_eq!(found, Some(LeaseId::new(5, 7)));
    assert_eq!(store.lease_scans(), scans + 1);
}

/// Asserts that an index row that names the lease of another operation is not believed
/// either: the operation's own lease is found.
#[test]
fn an_index_row_naming_the_lease_of_another_operation_is_not_believed() {
    let store = Racing::new();
    let durable = durable_over(&store);
    running(&durable);
    submit(&durable, 5, 2).expect("queued");
    grant(&durable, 5, 8, "node-a").expect("a grant");
    overwrite(&store, INDEX_4, pair_row(5, 8).to_vec());

    let found = lease_found(submit(&durable, 9, 1));

    assert_eq!(found, Some(LeaseId::new(5, 7)));
}

/// Asserts that an index row that is not a lease id is refused naming the row.
#[test]
fn an_index_row_that_is_not_a_lease_id_is_refused_naming_it() {
    let store = Racing::new();
    let durable = durable_over(&store);
    running(&durable);
    overwrite(&store, INDEX_4, vec![1, 2, 3]);

    let refused = submit(&durable, 9, 1).expect_err("a damaged index row");

    let said = refused.to_string();
    assert!(said.contains(INDEX_4), "{said}");
    assert!(said.contains("not a 16-byte lease id"), "{said}");
}

/// Asserts that ending an operation deletes the index row whoever left it: a refusal of an
/// operation that holds no lease deletes a row a release that predates the index left.
#[test]
fn ending_an_operation_deletes_a_stale_index_row() {
    let store = Racing::new();
    let durable = durable_over(&store);
    submit(&durable, 4, 1).expect("queued");
    overwrite(&store, INDEX_4, pair_row(5, 7).to_vec());
    let refusal = Refusal {
        operation: OperationId(4),
        waiters: Vec::new(),
        reason: "no worker".to_owned(),
    };
    let ended = ending("refused");

    durable
        .persist(&[Effect::Refuse(refusal)], Some(&ended))
        .expect("refused");

    assert_eq!(rows(&store, "lease-by-op/"), 0);
}

/// Catches a restart that forgets an operation had a lease: a farm that recovers operation 4
/// under a lease (taken up when its node says `Hello` here) counts it as started, so if it
/// is queued again its streams read `WaitingKept` (a drain keeps them), not `Waiting`.
#[test]
fn a_recovered_operation_with_a_lease_counts_as_started() {
    let store = Racing::new();
    let durable = durable_over(&store);
    running(&durable);
    drop(durable);

    let farm = farm_over(&store);
    // The leased operation is taken up when its node says `Hello` here.
    let _inbox = super::intake_tests::register(&farm, "node-a");
    let started = farm.lock().ever_started.clone();
    assert_eq!(started, [OperationId(4)].into());
}
