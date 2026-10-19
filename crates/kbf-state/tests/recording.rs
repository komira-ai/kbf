//! `RecordingStore`: forwards everything, and remembers each write it was asked for.
//!
//! Every test names the defect it catches.

use std::sync::Arc;
use std::time::Duration;

use kbf_state::fake::{Fake, Flaw};
use kbf_state::recording::{RecordingStore, Write, WriteKind};
use kbf_state::{
    Cond, Fenced, GENERATION, ManualClock, Revision, ScanRange, Store, StoreError, Txn,
};

fn fake() -> Arc<dyn Store> {
    Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ))
}

fn write(key: &str, kind: WriteKind, committed: bool) -> Write {
    Write {
        key: key.to_owned(),
        kind,
        committed,
    }
}

// Catches a recorder that misses a kind of write or misreports it: puts (with and without
// a time to live), deletes and counters are recorded in order with their kind; a check
// is a precondition, not a write, and is not.
#[test]
fn every_kind_of_write_is_recorded_in_order_and_a_check_is_not() {
    let recording = RecordingStore::new(fake());
    recording
        .txn(
            Txn::new()
                .put("a", "1", Cond::Any)
                .put_ttl("b", "2", Duration::from_secs(5), Cond::Any)
                .check("watched", Cond::Any)
                .delete("a", Cond::Any)
                .next("n"),
        )
        .expect("store")
        .expect("commit");
    assert_eq!(
        recording.writes(),
        [
            write("a", WriteKind::Put, true),
            write("b", WriteKind::Put, true),
            write("a", WriteKind::Delete, true),
            write("n", WriteKind::Next, true),
        ]
    );
    assert_eq!(
        recording.keys().into_iter().collect::<Vec<_>>(),
        ["a", "b", "n"]
    );
}

// Catches a recorder that forgets what was refused: a write whose precondition fails is
// recorded as not committed (the keys a component tried to write are what a test needs),
// and in a batch each transaction has its own outcome.
#[test]
fn a_refused_write_is_recorded_as_not_committed() {
    let recording = RecordingStore::new(fake());
    let results = recording
        .apply_batch(vec![
            Txn::new().put("k", "v", Cond::Absent),
            Txn::new().put("k", "w", Cond::Absent),
            Txn::new().put("z", "x", Cond::Any),
        ])
        .expect("store");
    assert!(results[0].is_ok() && results[1].is_err() && results[2].is_ok());
    assert_eq!(
        recording.writes(),
        [
            write("k", WriteKind::Put, true),
            write("k", WriteKind::Put, false),
            write("z", WriteKind::Put, true),
        ]
    );
}

// Catches a recorder that records only what succeeds: a store that fails the whole call
// (here a superseded fence) still has the write recorded, as not committed, and the error
// reaches the caller unchanged.
#[test]
fn a_write_the_store_fails_is_recorded_and_the_error_passes_through() {
    let inner = fake();
    let old = Fenced::take(Arc::clone(&inner), None).expect("take");
    Fenced::take(inner, Some(1)).expect("the rival takes");
    let recording = RecordingStore::new(Arc::new(old));
    let err = recording
        .txn(Txn::new().put("mine", "v", Cond::Any))
        .unwrap_err();
    assert_eq!(err, StoreError::Fenced);
    assert_eq!(recording.writes(), [write("mine", WriteKind::Put, false)]);
}

// Catches a recorder that sees the fence's own plumbing as the component's writes: under a
// fenced handle the generation check is appended to every transaction, and the recorded
// keys are only the component's. Put on the other side of the fence, the take itself is
// the only write of the generation row.
#[test]
fn a_fence_above_the_recorder_adds_no_keys_and_a_take_below_it_shows() {
    let recording = Arc::new(RecordingStore::new(fake()));
    let held = Fenced::take(Arc::clone(&recording) as Arc<dyn Store>, None).expect("take");
    assert_eq!(
        recording.keys().into_iter().collect::<Vec<_>>(),
        [GENERATION]
    );
    recording.clear();
    held.txn(Txn::new().put("p/1", "v", Cond::Any))
        .expect("store")
        .expect("commit");
    assert_eq!(recording.writes(), [write("p/1", WriteKind::Put, true)]);
}

// Catches reaps going unseen (they write Expire rows for keys nobody names) and clear
// not clearing: reaps are counted on both reap calls, and clear resets writes and reaps.
#[test]
fn reaps_are_counted_and_clear_forgets_everything() {
    let recording = RecordingStore::new(fake());
    recording
        .txn(Txn::new().put("k", "v", Cond::Any))
        .expect("store")
        .expect("commit");
    assert_eq!(recording.reap_expired(), Ok(0));
    assert_eq!(recording.reap_expired_if("k", Cond::Any), Ok(Ok(0)));
    assert_eq!(recording.reaps(), 2);
    recording.clear();
    assert_eq!((recording.reaps(), recording.writes()), (0, Vec::new()));
}

// Catches a wrapper that changes what it forwards: reads, the feed, the clock, the
// counters and the calls a backend may refuse all behave as the store underneath, and
// none of them is recorded.
#[test]
fn everything_else_is_forwarded_and_not_recorded() {
    let inner = fake();
    let recording = RecordingStore::new(Arc::clone(&inner));
    recording
        .txn(
            Txn::new()
                .put("p/a", "1", Cond::Any)
                .put("p/b", "2", Cond::Any),
        )
        .expect("store")
        .expect("commit");
    recording.clear();
    assert_eq!(
        recording.get("p/a").expect("get"),
        inner.get("p/a").expect("get")
    );
    assert_eq!(recording.scan_prefix("p/").expect("scan").len(), 2);
    let page = recording
        .scan_range(ScanRange::new("p/", std::num::NonZeroUsize::MIN))
        .expect("range");
    assert_eq!(page.rows.len(), 1);
    assert_eq!(
        recording
            .changes(Revision::ZERO, 10)
            .expect("feed")
            .rows
            .len(),
        2
    );
    assert_eq!(recording.commit_stats(), inner.commit_stats());
    assert_eq!(recording.now(), inner.now());
    assert!(recording.demote().is_err(), "the fake has no writer role");
    assert!(recording.hold_liveness(1).is_err());
    assert!(recording.holder_alive(1).is_err());
    assert_eq!(recording.writes(), Vec::new());
}
