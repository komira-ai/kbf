//! The slot bookkeeping and its recovery thread, with a stand-in for the clean: no user
//! database, no Mac, no runtime beyond what a test makes.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use futures::FutureExt as _;

use super::*;

/// A subscriber that drops what it is told, so that the log lines of the book run (and
/// count) in every test; the first call in the process wins.
fn quiet() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(std::io::sink)
        .try_init();
}

struct Stub(u32);

impl Slotted for Stub {
    fn uid(&self) -> u32 {
        700 + self.0
    }
}

fn stubs(count: u32) -> Vec<Stub> {
    (0..count).map(Stub).collect()
}

fn dirty() -> Outcome {
    Outcome::Dirty {
        why: "test".to_owned(),
    }
}

/// A recovery that waits 10 ms and at most 80 ms.
const FAST: Backoff = Backoff {
    first: Duration::from_millis(10),
    max: Duration::from_millis(80),
};

/// A recovery that does not run in the time a test takes: the test calls `retry`.
const NEVER: Backoff = Backoff {
    first: Duration::from_secs(3600),
    max: Duration::from_secs(3600),
};

const LONG: Duration = Duration::from_secs(10);

/// A book over `count` slots cleaned by a stand-in that answers `outcomes` in turn (then
/// clean) and counts its calls.
fn book(
    count: u32,
    outcomes: Vec<Outcome>,
    backoff: Backoff,
) -> (Arc<Book<Stub>>, Arc<AtomicUsize>) {
    quiet();
    let calls = Arc::new(AtomicUsize::new(0));
    let queue = Mutex::new(VecDeque::from(outcomes));
    let counted = Arc::clone(&calls);
    let cleaner: Cleaner<Stub> = Arc::new(move |_: &Stub| {
        counted.fetch_add(1, Ordering::SeqCst);
        lock(&queue).pop_front().unwrap_or(Outcome::Clean)
    });
    (Book::start(stubs(count), cleaner, backoff), calls)
}

/// The counts the hook of `book` is called with, in order.
fn counts(book: &Book<Stub>) -> mpsc::Receiver<usize> {
    let (send, receive) = mpsc::channel();
    let send = Mutex::new(send);
    book.on_count(Arc::new(move |n| {
        let _ = lock(&send).send(n);
    }));
    receive
}

/// Reads counts until one equals `want`; the ones before it.
fn until(receive: &mpsc::Receiver<usize>, want: usize) -> Result<Vec<usize>, Vec<usize>> {
    let mut seen = Vec::new();
    while let Ok(n) = receive.recv_timeout(LONG) {
        seen.push(n);
        if n == want {
            return Ok(seen);
        }
    }
    Err(seen)
}

/// Catches a slot that goes back to the pool uncleaned or although its clean left it
/// dirty (the next lease would find the last one's state), a dirty slot the node does
/// not report as one fewer, and a clean slot that does not go back: a clean slot is
/// taken again at once; a dirty one is not offered, and is reported one fewer, to the
/// hook as well.
#[tokio::test]
async fn a_dirty_slot_is_not_offered_and_is_reported_one_fewer() {
    // Clean at the start, then dirty at every clean after it.
    let outcomes = [vec![Outcome::Clean], vec![dirty(); 50]].concat();
    let (book, calls) = book(1, outcomes, NEVER);
    let reported = counts(&book);
    assert_eq!(reported.recv_timeout(LONG), Ok(1));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the start cleans each slot"
    );
    let (index, permit) = book.take().await.expect("a clean slot is taken");
    assert_eq!(index, 0);
    book.held(index, permit).give();
    assert_eq!(calls.load(Ordering::SeqCst), 2, "giving back cleans");
    assert_eq!(book.count(), 0, "a slot whose clean failed is reported");
    assert_eq!(reported.recv_timeout(LONG), Ok(0), "the hook is told");
    assert_eq!(book.dirty(), BTreeSet::from([0]));
    assert!(
        book.take().now_or_never().is_none(),
        "a dirty slot was offered"
    );
    assert!(lock(&book.free).is_empty(), "it is in the free set");
}

/// Catches a node whose slots are all dirty that never recovers (it takes no lease, so no
/// lease would trigger a re-clean, and its report does not change): with no lease, no
/// runtime and no report, the recovery thread cleans the slot again and the hook is told
/// the count. The mutants "no recovery thread" and "the count not pushed to the hook"
/// both time out here: the hook never hears 1.
#[test]
fn a_dirty_node_recovers_by_itself_and_says_so() {
    quiet();
    let gate = Arc::new(AtomicBool::new(false));
    let open = Arc::clone(&gate);
    let cleaner: Cleaner<Stub> = Arc::new(move |_: &Stub| {
        if open.load(Ordering::SeqCst) {
            Outcome::Clean
        } else {
            dirty()
        }
    });
    // From a plain thread: this test has no runtime.
    let book = Book::start(stubs(2), cleaner, FAST);
    let reported = counts(&book);
    assert_eq!(reported.recv_timeout(LONG), Ok(0), "every slot is dirty");
    gate.store(true, Ordering::SeqCst);
    let seen = until(&reported, 2).expect("the report never showed the slots back");
    assert!(seen.iter().all(|n| *n <= 2), "{seen:?}");
    assert_eq!(book.count(), 2);
    assert!(book.dirty().is_empty());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let (index, _permit) = runtime
        .block_on(book.take())
        .expect("a recovered slot is offered");
    assert!(index < 2);
}

/// Catches a recovery that waits the same time for ever (a slot that stays dirty would be
/// cleaned every five seconds for good), one that never resets (a slot that comes back
/// and fails again would wait five minutes), and one with no cap: the wait doubles from
/// `first` to `max` while a pass frees nothing, and starts again from `first` after a
/// pass that freed a slot.
#[test]
fn the_backoff_doubles_to_its_cap_and_starts_again_after_a_slot_comes_back() {
    let backoff = Backoff::DAEMON;
    let mut wait = backoff.first;
    let mut waits = vec![wait];
    for _ in 0..8 {
        wait = backoff.next(wait, false);
        waits.push(wait);
    }
    let secs: Vec<u64> = waits.iter().map(Duration::as_secs).collect();
    assert_eq!(secs, [5, 10, 20, 40, 80, 160, 300, 300, 300]);
    assert_eq!(backoff.next(Duration::from_secs(160), true), backoff.first);
}

/// Catches a recovery thread whose real waits do not grow: with a cleaner that never
/// passes, the passes get further apart, so the fifth is later than five of the first
/// gap (10 ms) would be.
#[test]
fn a_slot_that_stays_dirty_is_cleaned_less_and_less_often() {
    quiet();
    let (send, receive) = mpsc::channel();
    let send = Mutex::new(send);
    let cleaner: Cleaner<Stub> = Arc::new(move |_: &Stub| {
        let _ = lock(&send).send(Instant::now());
        dirty()
    });
    let _book = Book::start(stubs(1), cleaner, FAST);
    let times: Vec<Instant> = (0..6)
        .map(|_| receive.recv_timeout(LONG).expect("a pass"))
        .collect();
    let gaps: Vec<Duration> = times.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(gaps[4] >= Duration::from_millis(60), "{gaps:?}");
    assert!(gaps[4] > gaps[0], "{gaps:?}");
}

/// Catches a slot that fails for the same reason over and over logged at `ERROR` each
/// time: a failure is news the first time and when its reason changes, and not again;
/// a clean in between makes the next failure news.
#[test]
fn a_repeated_failure_is_news_once() {
    let (book, _) = book(1, vec![Outcome::Clean], NEVER);
    assert!(book.note_failure(0, "a"));
    assert!(!book.note_failure(0, "a"));
    assert!(book.note_failure(0, "b"));
    assert!(!book.note_failure(0, "b"));
    // A pass that cleans the slot clears what it remembered.
    lock(&book.dirty).insert(0);
    assert_eq!(book.retry(), 1);
    assert!(book.note_failure(0, "b"));
}

/// Catches a lease that waits for a dirty slot for ever: it is offered the slot as soon as
/// the recovery thread has cleaned it, with no one asking.
#[tokio::test]
async fn a_waiting_lease_gets_the_slot_once_the_recovery_has_cleaned_it() {
    let (book, _) = book(1, vec![dirty(), dirty()], FAST);
    let (index, _permit) = tokio::time::timeout(LONG, book.take())
        .await
        .expect("a waiting lease got the slot once a clean passed")
        .expect("taken");
    assert_eq!(index, 0);
    assert_eq!(book.count(), 1);
}

/// Catches a recheck needing the other slots' leases to end (`retry` frees every dirty
/// slot, one at a time, whoever holds the rest): two dirty slots and a clean one held,
/// and one pass frees both.
#[tokio::test]
async fn one_pass_cleans_every_dirty_slot_while_another_is_held() {
    let (book, _) = book(3, vec![Outcome::Clean, dirty(), dirty()], NEVER);
    let (_held, _permit) = book.take().await.expect("slot 0");
    assert_eq!(book.count(), 1);
    assert_eq!(book.retry(), 2);
    assert_eq!(book.count(), 3);
    assert_eq!(book.dirty(), BTreeSet::new());
}

/// Catches a slot whose clean did not finish (the clean panicked, or its thread or task
/// never ran) counted as able to run a lease, with a permit that finds no free slot
/// (`take` would fail with "none free"), and a re-clean that panics again taking the
/// caller down: it is dirty, no lease waits on a permit for it, and a panic at the
/// re-clean leaves it dirty.
#[tokio::test]
async fn a_slot_whose_clean_did_not_finish_is_dirty() {
    quiet();
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let cleaner: Cleaner<Stub> = Arc::new(move |_: &Stub| {
        assert!(
            counted.fetch_add(1, Ordering::SeqCst) == 0,
            "the clean panics"
        );
        Outcome::Clean
    });
    let book = Book::start(stubs(1), cleaner, NEVER);
    let (index, permit) = book.take().await.expect("taken");
    let held = book.held(index, permit);
    assert_eq!(format!("{held:?}"), "Held { index: 0, .. }");
    let gave = std::thread::spawn(move || held.give()).join();
    assert!(gave.is_err(), "the clean's panic reaches the caller");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        book.count(),
        0,
        "a slot whose clean did not finish is reported"
    );
    // The re-clean panics too: caught, and the slot stays dirty.
    assert_eq!(book.retry(), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(book.dirty(), BTreeSet::from([0]));
    assert!(
        book.take().now_or_never().is_none(),
        "a permit was left for it"
    );
    assert_eq!(book.all().len(), 1);
    assert_eq!(book.ready(0).uid(), 700);
}

/// Catches two re-cleans of one slot at once (the recovery thread and another caller
/// would clean the same user beside each other): while one is cleaning, another does
/// nothing and returns. The cleaner times out its wait, so the mutant "no guard" fails
/// here (a second clean starts) instead of hanging.
#[test]
fn only_one_re_clean_runs_at_a_time() {
    let (entered, entered_rx) = mpsc::channel::<()>();
    let (release, release_rx) = mpsc::channel::<()>();
    let (entered, release_rx) = (Mutex::new(entered), Mutex::new(release_rx));
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let cleaner: Cleaner<Stub> = Arc::new(move |_: &Stub| {
        if counted.fetch_add(1, Ordering::SeqCst) == 0 {
            return dirty();
        }
        let _ = lock(&entered).send(());
        let _ = lock(&release_rx).recv_timeout(Duration::from_secs(2));
        Outcome::Clean
    });
    quiet();
    let book = Book::start(stubs(1), cleaner, NEVER);
    let first = std::thread::spawn({
        let book = Arc::clone(&book);
        move || book.retry()
    });
    entered_rx
        .recv_timeout(LONG)
        .expect("the first re-clean started");
    // The second call must come back while the first still cleans: it neither waits for
    // the first nor cleans beside it.
    let (second_done, second_rx) = mpsc::channel();
    let second = std::thread::spawn({
        let book = Arc::clone(&book);
        move || {
            let _ = second_done.send(book.retry());
        }
    });
    assert_eq!(
        second_rx.recv_timeout(Duration::from_secs(1)),
        Ok(0),
        "a second re-clean waited for the first or ran beside it"
    );
    second.join().expect("second");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "a second clean started");
    release.send(()).expect("release");
    assert_eq!(first.join().expect("first"), 1);
    assert_eq!(book.count(), 1);
}

/// Catches a recovery thread that outlives its book (one thread left behind for every
/// pool that is dropped, and a clean it could still run on slots nobody owns): once the
/// book is dropped the thread ends, whether it was idle (every slot clean) or waiting to
/// clean a dirty one.
#[test]
fn the_recovery_thread_ends_with_its_book() {
    for outcomes in [Vec::new(), vec![dirty(); 100]] {
        let (book, _) = book(1, outcomes, NEVER);
        let wake = book.wake_handle();
        drop(book);
        // The thread's own handle goes when it returns.
        let give_up = Instant::now() + LONG;
        while Arc::strong_count(&wake) > 1 {
            assert!(
                Instant::now() < give_up,
                "the recovery thread is still running"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// Catches a slot given back clean that is not free again (the pool would shrink by one
/// permit every lease) and one that is marked dirty although its clean passed: a clean
/// slot goes back, is taken again, and the pool never counts it dirty.
#[tokio::test]
async fn a_slot_given_back_clean_is_free_again() {
    let (book, calls) = book(1, Vec::new(), NEVER);
    let (index, permit) = book.take().await.expect("taken");
    book.held(index, permit).give();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(book.count(), 1);
    assert!(book.dirty().is_empty());
    let (again, _permit) = book.take().await.expect("taken again");
    assert_eq!(again, 0);
}
