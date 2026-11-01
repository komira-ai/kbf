//! The build-only rows over `js-store`: what a submission, an ending and a look write and
//! read, and the size summaries.

use std::time::Duration;

use js_store::{Ending, Ext, StateError, Submitted};
use kbf_estimator::{Basis, Cfg, Instance, Key, PlatformClass, Shape};
use kbf_state::{Cond, Store, Txn};
use kbf_types::{ActionKey, OperationId};

use super::ext::KbfExt;
use super::testing::{overwrite, parcel, request, rows, store};
use crate::farm::durable::{estimate, named};

fn action() -> ActionKey {
    ActionKey::try_from(&request(1).key).expect("an action")
}

fn size_key() -> Key {
    Key::new(
        Instance::new("main"),
        PlatformClass::new("linux/x86_64"),
        Basis::Cgroup,
        Shape(0x0123_4567_89ab_cdef),
        Some(Cfg(7)),
    )
}

fn apply(store: &dyn Store, txn: Txn) {
    store.txn(txn).expect("a store").expect("applied");
}

/// Asserts that a submission puts the operation's `subject/` row with the parcel it was given,
/// an ending takes it away and puts the `doneof/` row with the action and the result's
/// lifetime, and that the parcels read back as written.
///
/// Catches: a row under another key, a parcel changed on the way in, a `subject/` row left
/// behind by an ending, and a `doneof/` row that does not live as long as the result.
#[test]
fn a_submission_and_an_ending_write_the_subject_and_doneof_rows() {
    let store = store();
    let rows = rows(&store, 1);
    let operation = rows.reserve_operation(OperationId(4)).expect("ids");
    rows.reserve_seqs(0).expect("sequences");
    let queued = rows.submit(operation, &request(1), &parcel(), None);
    assert!(matches!(queued, Ok(Submitted::Queued(id)) if id == operation));
    let key = format!("subject/{:016x}", operation.0);
    assert_eq!(store.get(&key).expect("read").expect("row").value, parcel());
    assert_eq!(
        KbfExt.parcel(&*store, operation).expect("parcel"),
        Some(parcel())
    );
    assert_eq!(
        KbfExt.parcels(&*store).expect("parcels").get(&operation),
        Some(&parcel())
    );
    assert_eq!(KbfExt.ended_parcel(&*store, operation).expect("none"), None);

    let ending = Ending::new(vec![1], Duration::from_secs(1)).with_parcel(named::encode(&action()));
    rows.refused(operation, &ending).expect("a refusal");
    assert!(store.get(&key).expect("read").is_none(), "subject kept");
    assert_eq!(KbfExt.parcel(&*store, operation).expect("none"), None);
    let done = store
        .get(&named::doneof_key(operation))
        .expect("read")
        .expect("a doneof row");
    assert_eq!(done.value, named::encode(&action()));
    assert!(done.expires_at.is_some(), "doneof lives as long as done");
    assert_eq!(
        store
            .get(&format!("done/{:016x}", operation.0))
            .expect("read")
            .expect("done")
            .expires_at,
        done.expires_at
    );
    assert_eq!(
        KbfExt.ended_parcel(&*store, operation).expect("parcel"),
        Some(named::encode(&action()))
    );
}

/// Asserts that a second submission of one operation id is refused over the first.
///
/// Catches: a `subject/` row put over another operation's (the first submission's subject
/// would be lost to the second).
#[test]
fn a_second_subject_row_for_one_operation_is_refused() {
    let store = store();
    let txn = KbfExt.submitted(Txn::new(), OperationId(9), &parcel());
    apply(&*store, txn);
    let again = KbfExt.submitted(Txn::new(), OperationId(9), b"other");
    let refused = store.txn(again).expect("a store");
    assert!(refused.is_err(), "the second put was taken");
    assert_eq!(
        KbfExt.parcel(&*store, OperationId(9)).expect("parcel"),
        Some(parcel())
    );
}

/// Asserts that a row that is not a subject, or not an action, is refused naming the row, by
/// the look at one operation and by the look at all of them; and that a `subject/` key that is
/// not an operation id is refused.
///
/// Catches: a corrupt row handed to the scheduler as a parcel.
#[test]
fn a_row_that_is_not_a_subject_or_an_action_is_refused_naming_it() {
    let store = store();
    let key = |error: StateError| match error {
        StateError::Corrupt { key, .. } => key,
        other => panic!("{other:?}"),
    };
    overwrite(&store, "subject/0000000000000005", vec![0xff]);
    let said = KbfExt.parcel(&*store, OperationId(5)).expect_err("subject");
    assert_eq!(key(said), "subject/0000000000000005");
    let said = KbfExt.parcels(&*store).expect_err("subjects");
    assert_eq!(key(said), "subject/0000000000000005");
    overwrite(&store, "doneof/0000000000000005", vec![0xff]);
    let said = KbfExt
        .ended_parcel(&*store, OperationId(5))
        .expect_err("action");
    assert_eq!(key(said), "doneof/0000000000000005");

    overwrite(&store, "subject/0000000000000005", parcel());
    overwrite(&store, "subject/zz", parcel());
    let said = KbfExt.parcels(&*store).expect_err("not an id");
    assert_eq!(key(said), "subject/zz");
}

/// Asserts that the size summaries are put under their `est/` keys, read back, and that a row
/// that cannot be read is skipped.
///
/// Catches: a summary under another key than the one a key floor names it by, and a bad row
/// that fails the read of the good ones.
#[test]
fn size_summaries_are_put_read_back_and_a_bad_row_is_skipped() {
    let store = store();
    let summary = b"summary".to_vec();
    let rows = [(size_key(), summary.clone())];
    apply(&*store, KbfExt.put_estimates(Txn::new(), &rows));
    let name = KbfExt.summary_name(&size_key());
    assert_eq!(name, estimate::row_key(&size_key()));
    let held = store.get(&name).expect("read").expect("a row");
    assert_eq!(held.value, estimate::encode(&summary));
    overwrite(&store, "est/not-a-key", estimate::encode(b"x"));
    let read = KbfExt.estimates(&*store).expect("estimates");
    assert_eq!(read, rows);
    let txn = Txn::new().delete(name, Cond::Any);
    apply(&*store, txn);
    assert!(KbfExt.estimates(&*store).expect("estimates").is_empty());
}
