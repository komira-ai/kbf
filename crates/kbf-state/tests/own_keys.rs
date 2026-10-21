//! `OwnKeys`: the handle of a superseded holder that may still write the keys a permit
//! names and nothing else.
//!
//! Every test names the defect it catches.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use kbf_state::fake::{Fake, Flaw};
use kbf_state::{
    Cond, Fenced, ManualClock, Op, Permit, Revision, ScanRange, Store, StoreError, Txn,
};

fn fake() -> Arc<dyn Store> {
    Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ))
}

/// Accepts a put of a key under `mine/` that must create it, and nothing else.
fn mine() -> Permit {
    Arc::new(
        |op: &Op| matches!(op, Op::Put { key, cond: Cond::Absent, .. } if key.starts_with("mine/")),
    )
}

/// A holder that a successor has superseded, and its own-keys handle.
fn superseded() -> (Arc<dyn Store>, Arc<dyn Store>) {
    let base = fake();
    let old = Fenced::take(Arc::clone(&base), None).expect("generation 1");
    let own = old.own_keys(mine()).expect("a fenced handle makes one");
    Fenced::take(Arc::clone(&base), Some(1)).expect("the successor takes generation 2");
    (base, own)
}

// Catches an exemption that is not one: the superseded holder writes a key the permit
// names (an insert under its own prefix) although the successor holds the generation.
#[test]
fn a_permitted_write_lands_after_the_holder_is_superseded() {
    let (base, own) = superseded();
    own.txn(Txn::new().put("mine/a", "v", Cond::Absent))
        .expect("store")
        .expect("no conflict");
    assert_eq!(
        base.get("mine/a").expect("read").expect("a row").value,
        b"v"
    );
}

// Catches an exemption that lets a foreign write through: a key the permit does not
// name, a permitted key written with another precondition, a delete, a counter and a
// transaction that mixes a permitted key with a foreign one are all refused by the fence,
// and none of their keys, the permitted one in the mixed transaction included, is
// written. The last is the guard against a handle that writes the permitted half.
#[test]
fn everything_the_permit_does_not_name_is_fenced_and_a_mixed_transaction_is_all_fenced() {
    let (base, own) = superseded();
    let foreign = [
        Txn::new().put("op/1", "v", Cond::Absent),
        Txn::new().put("mine/b", "v", Cond::Any),
        Txn::new().delete("mine/c", Cond::Any),
        Txn::new().next("mine/counter"),
        Txn::new()
            .put("mine/d", "v", Cond::Absent)
            .put("op/2", "v", Cond::Absent),
        Txn::new(),
    ];
    for txn in foreign {
        assert_eq!(
            own.txn(txn.clone()).unwrap_err(),
            StoreError::Fenced,
            "{txn:?}"
        );
    }
    for key in ["op/1", "mine/b", "mine/d", "op/2", "mine/counter"] {
        assert_eq!(base.get(key).expect("read"), None, "{key} was written");
    }
}

// Catches a handle that routes by the first transaction, or loses the order of the
// results: a batch of a permitted insert that conflicts, a foreign write that a holder's
// fence lets through and a permitted insert that commits answers each in its own place.
#[test]
fn a_batch_is_answered_per_transaction_in_its_own_order() {
    let base = fake();
    let holder = Fenced::take(Arc::clone(&base), None).expect("generation 1");
    let own = holder.own_keys(mine()).expect("a fenced handle makes one");
    own.txn(Txn::new().put("mine/a", "first", Cond::Absent))
        .expect("store")
        .expect("created");
    let results = own
        .apply_batch(vec![
            Txn::new().put("mine/a", "second", Cond::Absent),
            Txn::new().put("op/1", "v", Cond::Any),
            Txn::new().put("mine/b", "v", Cond::Absent),
        ])
        .expect("store");
    assert!(
        results[0].is_err(),
        "the insert of an existing key conflicts"
    );
    assert!(results[1].is_ok() && results[2].is_ok());
    assert_eq!(
        base.get("mine/a").expect("read").expect("a row").value,
        b"first"
    );
    assert!(base.get("op/1").expect("read").is_some());
    assert!(own.apply_batch(Vec::new()).expect("store").is_empty());
}

// Catches an exemption that is available to a store with nothing to be exempt from: the
// fake, and a handle made over a fenced one, have no generation of their own to skip,
// and a wrapper that does not forward the call gives none.
#[test]
fn only_a_fenced_handle_makes_one() {
    assert!(fake().own_keys(mine()).is_none());
}

// Catches a handle that does not read, scan, reap or count as the fenced handle does: its
// reads see the successor's writes, a reap is fenced (it writes keys the permit cannot
// name) and the calls of the rest of the trait reach the store beneath.
#[test]
fn reads_and_the_rest_of_the_store_go_to_the_fenced_handle() {
    let base = fake();
    let holder = Fenced::take(Arc::clone(&base), None).expect("generation 1");
    let own = holder.own_keys(mine()).expect("a fenced handle makes one");
    base.txn(Txn::new().put_ttl("session/w", "v", Duration::from_secs(1), Cond::Any))
        .expect("store")
        .expect("put");
    assert!(own.get("session/w").expect("read").is_some());
    assert_eq!(own.scan_prefix("session/").expect("scan").len(), 1);
    let page = own
        .scan_range(ScanRange::new("session/", NonZeroUsize::new(10).unwrap()))
        .expect("range");
    assert_eq!(page.rows.len(), 1);
    assert!(own.changes(Revision::ZERO, 10).expect("feed").rows.len() >= 2);
    assert_eq!(own.reap_expired().expect("reap"), 0);
    assert!(own.reap_expired_if("meta/generation", Cond::Any).is_err());
    assert!(own.hold_liveness(1).is_err());
    assert!(own.holder_alive(1).is_err());
    assert!(own.demote().is_err());
    let _ = (own.commit_stats(), own.now());
    Fenced::take(Arc::clone(&base), Some(1)).expect("the successor takes");
    assert_eq!(own.reap_expired().unwrap_err(), StoreError::Fenced);
}
