//! The slot bookkeeping, with a stand-in for the clean: no user database, no Mac.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use super::*;

struct Stub(u32);

impl Slotted for Stub {
    fn uid(&self) -> u32 {
        self.0
    }
}

fn stubs(count: u32) -> Vec<Stub> {
    (0..count).map(|i| Stub(700 + i)).collect()
}

fn dirty() -> Outcome {
    Outcome::Dirty {
        why: "test".to_owned(),
    }
}

/// A book over `count` slots cleaned by a stand-in that answers `outcomes` in turn
/// (then clean) and counts its calls.
fn book(count: u32, outcomes: Vec<Outcome>) -> (Arc<Book<Stub>>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let queue = Mutex::new(VecDeque::from(outcomes));
    let counted = Arc::clone(&calls);
    let cleaner: Cleaner<Stub> = Arc::new(move |_: &Stub| {
        counted.fetch_add(1, Ordering::SeqCst);
        lock(&queue).pop_front().unwrap_or(Outcome::Clean)
    });
    (Arc::new(Book::new(stubs(count), cleaner)), calls)
}

/// Catches a slot that goes back to the pool uncleaned or although its clean left it
/// dirty (the next lease would find the last one's state), a dirty slot the node does
/// not report as one fewer, and a clean slot that does not go back: a clean slot is
/// taken again at once; a dirty one is not offered, and is reported one fewer.
#[tokio::test]
async fn a_dirty_slot_is_not_offered_and_is_reported_one_fewer() {
    // Clean at the start, then dirty at every clean after it.
    let outcomes = [vec![Outcome::Clean], vec![dirty(); 50]].concat();
    let (book, calls) = book(1, outcomes);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the start cleans each slot"
    );
    assert_eq!(book.count(), 1);
    let (index, permit) = book.take().await.expect("a clean slot is taken");
    assert_eq!(index, 0);
    book.held(index, permit).give();
    assert_eq!(calls.load(Ordering::SeqCst), 2, "giving back cleans");
    assert_eq!(book.count(), 0, "a slot whose clean failed is reported");
    assert_eq!(book.dirty(), BTreeSet::from([0]));
    let again = tokio::time::timeout(Duration::from_millis(300), book.take()).await;
    assert!(again.is_err(), "a dirty slot was offered: {again:?}");
    assert!(lock(&book.free).is_empty(), "it is in the free set");
}

/// Catches a user left dirty for good (the one failed step would take the slot out of
/// the pool until the daemon restarts, which is the quarantine this replaced) and a
/// retry that offers a slot before a clean passed: a dirty slot is cleaned again when a
/// lease waits for one and every slot is held, and is offered when that passes; when it
/// fails it stays dirty and the count does not move.
#[tokio::test]
async fn a_dirty_slot_is_cleaned_again_and_offered_once_a_clean_passes() {
    // Slot 0 clean at the start, slot 1 dirty; the second look at slot 1 fails, the
    // third passes.
    let (book, calls) = book(2, vec![Outcome::Clean, dirty(), dirty()]);
    assert_eq!(book.count(), 1);
    let (_, held) = book.take().await.expect("slot 0");
    // The only clean slot is held: this lease waits, and slot 1 is cleaned again (and
    // fails again).
    let waiting = tokio::spawn({
        let book = Arc::clone(&book);
        async move { book.take().await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(book.count(), 1, "a failed re-clean is not a slot back");
    assert!(!waiting.is_finished());
    // The next trigger passes.
    book.recheck();
    let (index, _permit) = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the waiting lease got the slot once it was clean")
        .expect("joined")
        .expect("taken");
    assert_eq!(index, 1);
    assert_eq!(book.count(), 2);
    assert!(book.dirty().is_empty());
    drop(held);
}

/// Catches a slot given back that does not clean the other dirty slots (a node whose
/// leases keep arriving on one slot would never recover another): the clean of the
/// returned slot is followed by the re-clean of the dirty one, which is free after it.
#[tokio::test]
async fn giving_a_slot_back_cleans_the_dirty_ones_again() {
    let (book, calls) = book(2, vec![Outcome::Clean, dirty()]);
    assert_eq!(book.count(), 1);
    let (index, permit) = book.take().await.expect("slot 0");
    book.held(index, permit).give();
    assert_eq!(calls.load(Ordering::SeqCst), 4, "slot 0, then slot 1 again");
    assert_eq!(book.count(), 2);
    let mut taken = BTreeSet::new();
    for _ in 0..2 {
        let (index, permit) = book.take().await.expect("both are free");
        permit.forget();
        taken.insert(index);
    }
    assert_eq!(taken, BTreeSet::from([0, 1]));
}

/// Catches a node whose slots are all dirty that never recovers (it takes no lease, so
/// no lease would trigger a re-clean): the report's recheck cleans them in the
/// background, and a lease waiting for a slot gets the first that passes. Also that a
/// recheck with nothing dirty cleans nothing.
#[tokio::test]
async fn a_node_report_cleans_dirty_slots_when_no_lease_comes() {
    let (book, calls) = book(1, vec![dirty()]);
    assert_eq!(book.count(), 0);
    let waiting = tokio::spawn({
        let book = Arc::clone(&book);
        async move { book.take().await }
    });
    // The wait itself cleaned it again (no permit was free): it passes.
    let (index, permit) = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the lease waited for a slot that is clean again")
        .expect("joined")
        .expect("taken");
    assert_eq!((index, calls.load(Ordering::SeqCst)), (0, 2));
    drop(permit);
    book.recheck();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2, "nothing was dirty");
}

/// Catches a recheck that needs a runtime to exist (the report is built from plain
/// threads too) and a dirty slot taken as clean without one: with no runtime it does
/// nothing and the slot stays dirty.
#[test]
fn a_recheck_without_a_runtime_does_nothing() {
    let (book, calls) = book(1, vec![dirty()]);
    book.recheck();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(book.count(), 0);
    assert_eq!(book.all().len(), 1);
    assert_eq!(book.ready(0).uid(), 700);
}

/// Catches a slot whose clean did not finish (the clean panicked, or its thread or task
/// never ran) counted as able to run a lease, with a permit that finds no free slot
/// (`take` would fail with "none free"), and a re-clean that panics again taking the
/// caller down: it is dirty, no lease waits on a permit for it, and a panic at the
/// re-clean leaves it dirty.
#[tokio::test]
async fn a_slot_whose_clean_did_not_finish_is_dirty() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let cleaner: Cleaner<Stub> = Arc::new(move |_: &Stub| {
        assert!(
            counted.fetch_add(1, Ordering::SeqCst) == 0,
            "the clean panics"
        );
        Outcome::Clean
    });
    let book = Arc::new(Book::new(stubs(1), cleaner));
    let (index, permit) = book.take().await.expect("taken");
    let held = book.held(index, permit);
    let gave = std::thread::spawn(move || held.give()).join();
    assert!(gave.is_err(), "the clean's panic reaches the caller");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        book.count(),
        0,
        "a slot whose clean did not finish is reported"
    );
    // The re-clean panics too: caught, and the slot stays dirty.
    book.retry(None);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(book.dirty(), BTreeSet::from([0]));
    let again = tokio::time::timeout(Duration::from_millis(300), book.take()).await;
    assert!(again.is_err(), "a permit was left for it: {again:?}");
}

/// Catches two re-cleans of one slot at once (two hand-backs would clean the same user
/// beside each other): while one is cleaning, another does nothing and returns.
#[test]
fn only_one_re_clean_runs_at_a_time() {
    let (entered, entered_rx) = std::sync::mpsc::channel::<()>();
    let (release, release_rx) = std::sync::mpsc::channel::<()>();
    let (entered, release_rx) = (Mutex::new(entered), Mutex::new(release_rx));
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let cleaner: Cleaner<Stub> = Arc::new(move |_: &Stub| {
        if counted.fetch_add(1, Ordering::SeqCst) == 0 {
            return dirty();
        }
        lock(&entered).send(()).expect("entered");
        lock(&release_rx).recv().expect("released");
        Outcome::Clean
    });
    let book = Arc::new(Book::new(stubs(1), cleaner));
    let first = std::thread::spawn({
        let book = Arc::clone(&book);
        move || book.retry(None)
    });
    entered_rx.recv().expect("the first re-clean started");
    book.retry(None);
    assert_eq!(calls.load(Ordering::SeqCst), 2, "a second re-clean started");
    release.send(()).expect("release");
    first.join().expect("first");
    assert_eq!(book.count(), 1);
}

/// Catches a lease that waits for a dirty slot on a clean nobody is running again (the
/// node's report and a hand-back were the only triggers, and a lease that arrived just
/// as a re-clean began would wait on it for ever once it failed): a waiting lease has the
/// dirty slot cleaned again every interval, and gets it the first time a clean passes.
#[tokio::test]
async fn a_waiting_lease_has_the_dirty_slot_cleaned_again_until_it_passes() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let cleaner: Cleaner<Stub> = Arc::new(move |_: &Stub| {
        // The start and the first two re-cleans fail; the third passes.
        if counted.fetch_add(1, Ordering::SeqCst) < 3 {
            dirty()
        } else {
            Outcome::Clean
        }
    });
    let book = Arc::new(Book::new(stubs(1), cleaner).rechecking_every(Duration::from_millis(50)));
    let (index, _permit) = tokio::time::timeout(Duration::from_secs(10), book.take())
        .await
        .expect("a waiting lease got the slot once a clean passed")
        .expect("taken");
    assert_eq!(index, 0);
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert_eq!(book.count(), 1);
}
