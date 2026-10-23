//! The `update/*` rows over a store that races or answers wrongly: a writer that wins between
//! a reader's look and its write (the compare-and-set loops of `offer` and `set_paused` read
//! again, and give up when the writers do not stop), and a store that returns no value for a
//! counter it advanced.
//!
//! Each test names the defect it would catch.

#[path = "update_support/mod.rs"]
mod support;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use kbf_server::update::{Desired, Policy, Release, RowError, Rows};
use kbf_state::{Cond, Store, StoreError, Txn};
use support::harness::{Backing, Cell};
use support::stores::{Meddle, writes};
use support::t;

const BACKINGS: [Backing; 2] = [Backing::Fake, Backing::Sqlite];

/// A rival that is called more often than this means the loop under test never gave up.
const NEVER_GAVE_UP: u64 = 500;

/// The policy row an operator's older build wrote: `auto_nodes` and a capacity floor of
/// `floor` (under 128, one byte), replacing the row whole.
fn operator_writes(store: &Arc<dyn Store>, floor: u8) {
    let row = vec![0x08, 0x01, 0x20, floor];
    store
        .txn(Txn::new().put("update/policy", row, Cond::Any))
        .expect("the operator's write")
        .expect("no condition");
}

/// Whether `error` is the store's own failure and its text names `what`.
fn store_says(error: &RowError, what: &str) -> bool {
    matches!(error, RowError::Store(StoreError::Failed(why)) if why.contains(what))
}

/// Counts the writes a hook is called for, and refuses to go on past [`NEVER_GAVE_UP`] of
/// them.
fn count(calls: &AtomicU64) -> u64 {
    let n = calls.fetch_add(1, Ordering::SeqCst);
    assert!(n < NEVER_GAVE_UP, "the loop under test never gave up");
    n
}

/// Catches: an offer that gives up when another offer lands between its read and its write,
/// or one that writes over the winner with the generation it drew before it. A rival offers
/// `rival` just before the first write of the run row; the offer reads again, draws a new
/// generation above the rival's and writes: the row is the offer's, and its generation is
/// above the winner's.
#[test]
fn an_offer_that_loses_a_race_reads_again_and_ends_above_the_winner() {
    for backing in BACKINGS {
        let cell = Cell::new(backing, "offer-lost");
        let inner = cell.store();
        let calls = Arc::new(AtomicU64::new(0));
        let won: Arc<Mutex<Option<Desired>>> = Arc::new(Mutex::new(None));
        let hook = {
            let (inner, calls, won) = (Arc::clone(&inner), Arc::clone(&calls), Arc::clone(&won));
            move |txn: &Txn| {
                if writes(txn, "update/desired") && count(&calls) == 0 {
                    let rival = Rows::new(Arc::clone(&inner))
                        .offer(&Release::new("rival"), t(5))
                        .expect("the rival's offer");
                    *won.lock().unwrap_or_else(PoisonError::into_inner) = Some(rival);
                }
            }
        };
        let rows = Rows::new(Meddle::before_each(Arc::clone(&inner), hook));
        let mine = rows.offer(&Release::new("mine"), t(9)).expect("an offer");
        let won = won
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .expect("the rival offered first");
        assert_eq!(mine.commit, Release::new("mine"), "{backing:?}");
        assert!(
            mine.generation > won.generation,
            "{backing:?}: {mine:?} is not above {won:?}"
        );
        let stored = Rows::new(inner).desired().unwrap().value;
        assert_eq!(stored, Some(mine), "{backing:?}: the winner was kept");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "one lost write, one made");
    }
}

/// Catches: an offer that never stops trying while another writer keeps winning, or one that
/// writes over the winner once it gives up. A rival offers a new release before every write
/// of the run row: the offer reads and tries again a bounded number of times, then reports
/// the row as contended, and the row is the last rival's.
#[test]
fn an_offer_that_keeps_losing_gives_up_and_writes_nothing_of_its_own() {
    for backing in BACKINGS {
        let cell = Cell::new(backing, "offer-contended");
        let inner = cell.store();
        let calls = Arc::new(AtomicU64::new(0));
        let hook = {
            let (inner, calls) = (Arc::clone(&inner), Arc::clone(&calls));
            move |txn: &Txn| {
                if writes(txn, "update/desired") {
                    let n = count(&calls);
                    let release = Release::new(format!("rival-{n}"));
                    Rows::new(Arc::clone(&inner))
                        .offer(&release, t(5))
                        .expect("a rival's offer");
                }
            }
        };
        let rows = Rows::new(Meddle::before_each(Arc::clone(&inner), hook));
        let error = rows.offer(&Release::new("mine"), t(9)).unwrap_err();
        assert_eq!(error, RowError::Contended("update/desired"), "{backing:?}");
        let tries = calls.load(Ordering::SeqCst);
        assert!(tries >= 2, "{backing:?}: it tried {tries} time(s)");
        let stored = Rows::new(inner).desired().unwrap().value.unwrap();
        assert_eq!(
            stored.commit,
            Release::new(format!("rival-{}", tries - 1)),
            "{backing:?}: an offer that gave up wrote"
        );
    }
}

/// Catches: a pause written from a read the policy has moved since, which would put back the
/// settings an operator changed in between (or lose the pause when the write is refused).
/// The operator writes a floor of three just before the first write of the policy row: the
/// pause reads again and sets `paused` on the operator's policy.
#[test]
fn a_pause_that_loses_a_race_reads_again_and_keeps_the_winners_policy() {
    for backing in BACKINGS {
        let cell = Cell::new(backing, "pause-lost");
        let inner = cell.store();
        let calls = Arc::new(AtomicU64::new(0));
        let hook = {
            let (inner, calls) = (Arc::clone(&inner), Arc::clone(&calls));
            move |txn: &Txn| {
                if writes(txn, "update/policy") && count(&calls) == 0 {
                    operator_writes(&inner, 3);
                }
            }
        };
        let rows = Rows::new(Meddle::before_each(Arc::clone(&inner), hook));
        rows.set_paused(true).expect("a pause");
        let want = Policy {
            min_serving: 3,
            paused: true,
            ..Policy::default()
        };
        assert_eq!(Rows::new(inner).policy().unwrap(), want, "{backing:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "one lost write, one made");
    }
}

/// Catches: a pause that never stops trying, or one that reports success when it wrote
/// nothing. The operator writes a new policy before every write of the policy row: the pause
/// gives up as contended, and the policy is not paused.
#[test]
fn a_pause_that_keeps_losing_gives_up_and_the_run_is_not_paused() {
    for backing in BACKINGS {
        let cell = Cell::new(backing, "pause-contended");
        let inner = cell.store();
        let calls = Arc::new(AtomicU64::new(0));
        let hook = {
            let (inner, calls) = (Arc::clone(&inner), Arc::clone(&calls));
            move |txn: &Txn| {
                if writes(txn, "update/policy") {
                    let n = count(&calls);
                    operator_writes(&inner, u8::try_from(n + 1).unwrap());
                }
            }
        };
        let rows = Rows::new(Meddle::before_each(Arc::clone(&inner), hook));
        let error = rows.set_paused(true).unwrap_err();
        assert_eq!(error, RowError::Contended("update/policy"), "{backing:?}");
        assert!(calls.load(Ordering::SeqCst) >= 2, "{backing:?}");
        assert!(!Rows::new(inner).policy().unwrap().paused, "{backing:?}");
    }
}

/// Catches: a store that advanced a counter and answered with no value for it being read as
/// the number zero. A generation of 0 is no run (a row that holds it is refused). The offer
/// fails with the store's error, and no run is written from a generation nobody drew.
#[test]
fn a_counter_the_store_returns_no_value_for_is_an_error_not_a_zero() {
    for backing in BACKINGS {
        let cell = Cell::new(backing, "no-counter");
        let rows = Rows::new(Meddle::without_counters(cell.store()));
        let error = rows.offer(&Release::new("v2"), t(1)).unwrap_err();
        assert!(store_says(&error, "counter"), "{backing:?}: {error:?}");
        let stored = Rows::new(cell.store()).desired().unwrap().value;
        assert_eq!(stored, None, "{backing:?}: a run was written");
    }
}

/// Catches: a `Debug` of the rows that prints the store, whose contents are not for a log
/// line, or one that stops compiling the day the store is not `Debug`. It is the type's name.
#[test]
fn the_rows_print_as_their_name_and_not_as_their_store() {
    let cell = Cell::new(Backing::Fake, "debug");
    assert_eq!(format!("{:?}", Rows::new(cell.store())), "Rows");
}
