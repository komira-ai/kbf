//! The `update/*` rows over a store that races or answers wrongly: a writer that wins between
//! a reader's look and its write (the compare-and-set loops of `offer`, `set_paused`, `claim`
//! and `release` read again, and give up when the writers do not stop), and a store that
//! returns no value for a counter it advanced.
//!
//! Each test names the defect it would catch. The mutants that were planted to see each
//! group go red are listed in the pull request.

#[path = "update_support/mod.rs"]
mod support;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use kbf_server::update::{Desired, OrderId, Policy, Release, RowError, Rows, SlotClass};
use kbf_state::{StoreError, Txn};
use support::harness::{Backing, Cell};
use support::stores::{Meddle, writes};
use support::{id, t};

const BACKINGS: [Backing; 2] = [Backing::Fake, Backing::Sqlite];

/// A rival that is called more often than this means the loop under test never gave up.
const NEVER_GAVE_UP: u64 = 500;

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
                    let operator = Policy {
                        min_serving: 3,
                        ..Policy::default()
                    };
                    Rows::new(Arc::clone(&inner))
                        .set_policy(&operator)
                        .expect("the operator's policy");
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
                    let operator = Policy {
                        min_serving: usize::try_from(n + 1).unwrap(),
                        ..Policy::default()
                    };
                    Rows::new(Arc::clone(&inner))
                        .set_policy(&operator)
                        .expect("the operator's policy");
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

/// Catches: a claim that gives up, or claims a place it does not hold, when other claims
/// keep landing between its read and its write. Another host claims a place before every
/// write of the slot row: the claim reads again each time, gives up as contended, and the
/// host holds no place.
#[test]
fn a_claim_that_keeps_losing_gives_up_and_holds_no_place() {
    for backing in BACKINGS {
        let cell = Cell::new(backing, "claim-contended");
        let inner = cell.store();
        let calls = Arc::new(AtomicU64::new(0));
        let hook = {
            let (inner, calls) = (Arc::clone(&inner), Arc::clone(&calls));
            move |txn: &Txn| {
                if writes(txn, "update/slot") {
                    let n = count(&calls);
                    let rival = id(&format!("r{n}"));
                    Rows::new(Arc::clone(&inner))
                        .claim(SlotClass::Host, &rival, OrderId::new(1, n + 1), 1_000)
                        .expect("a rival's claim");
                }
            }
        };
        let rows = Rows::new(Meddle::before_each(Arc::clone(&inner), hook));
        let mine = OrderId::new(1, 9_999);
        let error = rows
            .claim(SlotClass::Host, &id("me"), mine, 1_000)
            .unwrap_err();
        assert_eq!(error, RowError::Contended("update/slot"), "{backing:?}");
        let slot = Rows::new(inner).slot().unwrap().value;
        assert!(!slot.holds(&id("me")), "{backing:?}: {slot:?}");
        assert!(slot.holds(&id("r0")), "{backing:?}: {slot:?}");
    }
}

/// Catches: a release that gives up and says the place is free, or one that never stops, when
/// other claims keep landing between its read and its write. `me` holds a place; another host
/// claims one before every write of the slot row: the release gives up as contended and `me`
/// still holds its place (the caller is told, and does not believe it is free).
#[test]
fn a_release_that_keeps_losing_gives_up_and_the_place_stays_held() {
    for backing in BACKINGS {
        let cell = Cell::new(backing, "release-contended");
        let inner = cell.store();
        Rows::new(Arc::clone(&inner))
            .claim(SlotClass::Host, &id("me"), OrderId::new(1, 1), 1_000)
            .unwrap();
        let calls = Arc::new(AtomicU64::new(0));
        let hook = {
            let (inner, calls) = (Arc::clone(&inner), Arc::clone(&calls));
            move |txn: &Txn| {
                if writes(txn, "update/slot") {
                    let n = count(&calls);
                    let rival = id(&format!("r{n}"));
                    Rows::new(Arc::clone(&inner))
                        .claim(SlotClass::Host, &rival, OrderId::new(1, n + 2), 1_000)
                        .expect("a rival's claim");
                }
            }
        };
        let rows = Rows::new(Meddle::before_each(Arc::clone(&inner), hook));
        let error = rows.release(&id("me")).unwrap_err();
        assert_eq!(error, RowError::Contended("update/slot"), "{backing:?}");
        let slot = Rows::new(inner).slot().unwrap().value;
        assert!(slot.holds(&id("me")), "{backing:?}: {slot:?}");
        assert!(calls.load(Ordering::SeqCst) >= 2, "{backing:?}");
    }
}

/// Catches: a release that stops at the first lost race, and says the place is free when the
/// row it wrote was refused, or one that writes its stale view over the winner's claim.
/// Another host claims a place just before the first write of the slot row: the release reads
/// again and frees `me`, and the rival's place is still there.
#[test]
fn a_release_that_loses_a_race_reads_again_and_frees_the_place() {
    for backing in BACKINGS {
        let cell = Cell::new(backing, "release-lost");
        let inner = cell.store();
        Rows::new(Arc::clone(&inner))
            .claim(SlotClass::Host, &id("me"), OrderId::new(1, 1), 1_000)
            .unwrap();
        let calls = Arc::new(AtomicU64::new(0));
        let hook = {
            let (inner, calls) = (Arc::clone(&inner), Arc::clone(&calls));
            move |txn: &Txn| {
                if writes(txn, "update/slot") && count(&calls) == 0 {
                    Rows::new(Arc::clone(&inner))
                        .claim(SlotClass::Host, &id("rival"), OrderId::new(1, 2), 1_000)
                        .expect("the rival's claim");
                }
            }
        };
        let rows = Rows::new(Meddle::before_each(Arc::clone(&inner), hook));
        let freed = rows.release(&id("me")).expect("a release");
        assert!(!freed.holds(&id("me")), "{backing:?}: {freed:?}");
        assert!(freed.holds(&id("rival")), "{backing:?}: {freed:?}");
        let stored = Rows::new(inner).slot().unwrap().value;
        assert_eq!(stored, freed, "{backing:?}: the answer is the row");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "one lost write, one made");
    }
}

/// Catches: a store that fails the write of the slot being read as a place claimed or freed.
/// The store's error is the caller's, and the slot is as it was: the claim holds no place,
/// and the release leaves the place held (the caller must not believe it is free).
#[test]
fn a_store_that_fails_the_slot_write_is_the_callers_error_and_the_slot_is_unchanged() {
    for backing in BACKINGS {
        let cell = Cell::new(backing, "slot-fails");
        let inner = cell.store();
        Rows::new(Arc::clone(&inner))
            .claim(SlotClass::Host, &id("held"), OrderId::new(1, 1), 1_000)
            .unwrap();
        let rows = Rows::new(Meddle::failing_writes_to(Arc::clone(&inner), "update/slot"));

        let error = rows
            .claim(SlotClass::Host, &id("new"), OrderId::new(1, 2), 1_000)
            .unwrap_err();
        assert!(store_says(&error, "update/slot"), "{backing:?}: {error:?}");
        let error = rows.release(&id("held")).unwrap_err();
        assert!(store_says(&error, "update/slot"), "{backing:?}: {error:?}");

        let slot = Rows::new(inner).slot().unwrap().value;
        assert!(slot.holds(&id("held")), "{backing:?}: {slot:?}");
        assert!(!slot.holds(&id("new")), "{backing:?}: {slot:?}");
    }
}

/// Catches: a store that advanced a counter and answered with no value for it being read as
/// the number zero. A generation of 0 is no run (a row that holds it is refused), and an
/// order id of number 0 repeats the first id of its epoch. Both calls fail with the store's
/// error, and no run is written from a generation nobody drew.
#[test]
fn a_counter_the_store_returns_no_value_for_is_an_error_not_a_zero() {
    for backing in BACKINGS {
        let cell = Cell::new(backing, "no-counter");
        let rows = Rows::new(Meddle::without_counters(cell.store()));
        let error = rows.mint_order(3).unwrap_err();
        assert!(store_says(&error, "counter"), "{backing:?}: {error:?}");
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
