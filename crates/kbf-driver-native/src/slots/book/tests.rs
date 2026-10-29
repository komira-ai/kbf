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

/// A free slot of `book`, or a failure of the test (never a hang) after [`LONG`].
async fn take(book: &Arc<Book<Stub>>) -> (usize, OwnedSemaphorePermit) {
    tokio::time::timeout(LONG, book.take())
        .await
        .expect("no slot was offered in time")
        .expect("taken")
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
    let (index, permit) = take(&book).await;
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
/// the count: the hook hears the slots can run a lease again.
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
        .enable_time()
        .build()
        .expect("runtime");
    let (index, _permit) = runtime.block_on(take(&book));
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

/// A book whose recovery thread does not sleep: it asks for each wait on `delays` and
/// then holds until the test sends on the returned sender, so the test steps it one pass at
/// a time. The cleaner answers `outcomes` in turn (then clean) and counts its calls.
fn stepped(
    outcomes: Vec<Outcome>,
    backoff: Backoff,
) -> (
    Arc<Book<Stub>>,
    Arc<AtomicUsize>,
    mpsc::Receiver<Duration>,
    mpsc::Sender<()>,
) {
    quiet();
    let calls = Arc::new(AtomicUsize::new(0));
    let queue = Mutex::new(VecDeque::from(outcomes));
    let counted = Arc::clone(&calls);
    let cleaner: Cleaner<Stub> = Arc::new(move |_: &Stub| {
        counted.fetch_add(1, Ordering::SeqCst);
        lock(&queue).pop_front().unwrap_or(Outcome::Clean)
    });
    let (ask, delays) = mpsc::channel();
    let (go, stepping) = mpsc::channel::<()>();
    // A test that is done drops `go`: the waits then return at once and the thread ends
    // with its book.
    let nap: Nap = Box::new(move |_, time| {
        let _ = ask.send(time);
        let _ = stepping.recv();
        false
    });
    let book = Book::start_with(stubs(1), cleaner, backoff, None, nap);
    (book, calls, delays, go)
}

/// Catches a recovery thread that does not back off: with a cleaner that never passes,
/// the waits it asks for are the first, then double up to the cap and stay there (a
/// constant wait, none, or a wait without a cap would each differ), and one pass is made
/// per wait, none before the first.
#[test]
fn a_slot_that_stays_dirty_is_cleaned_less_and_less_often() {
    let (book, calls, delays, go) = stepped(vec![dirty(); 20], FAST);
    let mut asked = Vec::new();
    for passes_so_far in 0..7 {
        asked.push(delays.recv_timeout(LONG).expect("a wait"));
        // The start's clean, and one pass for each wait that was sat out.
        assert_eq!(calls.load(Ordering::SeqCst), 1 + passes_so_far);
        go.send(()).expect("step");
    }
    let millis: Vec<u128> = asked.iter().map(Duration::as_millis).collect();
    assert_eq!(millis, [10, 20, 40, 80, 80, 80, 80]);
    assert_eq!(book.dirty(), BTreeSet::from([0]));
}

/// Catches a pass that frees a slot not starting the waits again from the first while
/// another slot stays dirty (the thread is not idle, so only the pass's own accounting
/// resets the wait): slot 0 comes back at the third pass, slot 1 never, and the wait after
/// that pass is the first, not the next double.
#[test]
fn a_slot_that_comes_back_resets_the_wait_while_another_stays_dirty() {
    quiet();
    let zero_calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&zero_calls);
    // Slot 0 is dirty at the start and at two passes, clean at the third.
    let cleaner: Cleaner<Stub> = Arc::new(move |slot: &Stub| {
        if slot.0 == 0 && counted.fetch_add(1, Ordering::SeqCst) >= 3 {
            return Outcome::Clean;
        }
        dirty()
    });
    let (ask, delays) = mpsc::channel();
    let (go, stepping) = mpsc::channel::<()>();
    let nap: Nap = Box::new(move |_, time| {
        let _ = ask.send(time);
        let _ = stepping.recv();
        false
    });
    let book = Book::start_with(stubs(2), cleaner, FAST, None, nap);
    let mut asked = Vec::new();
    for _ in 0..4 {
        asked.push(delays.recv_timeout(LONG).expect("a wait"));
        go.send(()).expect("step");
    }
    let millis: Vec<u128> = asked.iter().map(Duration::as_millis).collect();
    assert_eq!(millis, [10, 20, 40, 10]);
    assert_eq!(book.dirty(), BTreeSet::from([1]));
}

/// Catches a recovery that keeps its grown wait after a slot came back (a slot that
/// fails again would wait the longest), and one that keeps cleaning a slot that is clean:
/// the waits grow while the slot stays dirty, the pass that cleans it ends them (no wait
/// is asked for, no clean is made), and when it goes dirty again the next wait is the first.
#[tokio::test]
async fn the_waits_stop_once_the_slot_is_clean_and_start_again_from_the_first() {
    // Dirty at the start and at the first two passes, clean at the third, dirty at the
    // lease's clean, clean after.
    let outcomes = vec![dirty(), dirty(), dirty(), Outcome::Clean, dirty()];
    let (book, calls, delays, go) = stepped(outcomes, FAST);
    let reported = counts(&book);
    assert_eq!(reported.recv_timeout(LONG), Ok(0));
    let mut asked = Vec::new();
    for _ in 0..3 {
        asked.push(delays.recv_timeout(LONG).expect("a wait"));
        go.send(()).expect("step");
    }
    until(&reported, 1).expect("the slot never came back");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        4,
        "the start and three passes"
    );
    let (index, permit) = take(&book).await;
    book.held(index, permit).give();
    assert_eq!(calls.load(Ordering::SeqCst), 5, "giving back cleans");
    asked.push(delays.recv_timeout(LONG).expect("a wait"));
    // Without the reset the wait would be the next double, 80 ms.
    let millis: Vec<u128> = asked.iter().map(Duration::as_millis).collect();
    assert_eq!(millis, [10, 20, 40, 10]);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        5,
        "no pass before the wait is sat out"
    );
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
    let (_held, _permit) = take(&book).await;
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
    let (index, permit) = take(&book).await;
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
/// nothing and returns. The cleaner times out its wait, so a regression fails here (a
/// second clean starts) instead of hanging.
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
    let (index, permit) = take(&book).await;
    book.held(index, permit).give();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(book.count(), 1);
    assert!(book.dirty().is_empty());
    let (again, _permit) = take(&book).await;
    assert_eq!(again, 0);
}

/// Catches a slot that goes dirty at run time and is not cleaned again for a minute (the
/// recovery thread asleep with nothing to wake it): a lease's clean fails, and the thread, idle until then,
/// cleans the slot again promptly and the count comes back.
#[tokio::test]
async fn a_slot_that_goes_dirty_at_run_time_is_recovered_promptly() {
    // Clean at the start, dirty at the lease's clean, clean at every clean after.
    let (book, _) = book(1, vec![Outcome::Clean, dirty()], FAST);
    let reported = counts(&book);
    assert_eq!(reported.recv_timeout(LONG), Ok(1));
    // Long enough for the recovery thread to be asleep with nothing dirty.
    std::thread::sleep(Duration::from_millis(200));
    let (index, permit) = take(&book).await;
    book.held(index, permit).give();
    let seen = until(&reported, 1).expect("the slot never came back");
    assert_eq!(
        seen,
        [0, 1],
        "down at the lease's clean, up at the recovery"
    );
}

/// Catches a start-up clean that panics taking the daemon down with it: a slot whose first
/// clean panics starts dirty, and the recovery thread brings it back.
#[test]
fn a_clean_that_panics_at_the_start_leaves_the_slot_dirty_and_it_recovers() {
    quiet();
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let cleaner: Cleaner<Stub> = Arc::new(move |_: &Stub| {
        assert!(
            counted.fetch_add(1, Ordering::SeqCst) > 0,
            "the first clean panics"
        );
        Outcome::Clean
    });
    let book = Book::start(stubs(1), cleaner, FAST);
    let reported = counts(&book);
    until(&reported, 1).expect("the slot never came back");
    assert!(calls.load(Ordering::SeqCst) >= 2);
}

/// Catches an older count overwriting a newer one at the receiver (the count read
/// outside the hook's lock): one change is held inside the hook,
/// after its count was read, while another happens; the receiver's last count is the
/// newest.
#[test]
fn a_stale_count_never_overwrites_a_newer_one() {
    quiet();
    // Slot 0 clean, slot 1 dirty: one slot can run a lease.
    let (book, _) = book(2, vec![Outcome::Clean, dirty()], NEVER);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    let (index, permit) = runtime.block_on(take(&book));
    let held = book.held(index, permit);
    let (entered, entered_rx) = mpsc::channel::<()>();
    let (release, release_rx) = mpsc::channel::<()>();
    let (entered, release_rx) = (Mutex::new(entered), Mutex::new(release_rx));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let calls = AtomicUsize::new(0);
    book.on_count(Arc::new(move |n| {
        // The call after the initial one stays inside the hook, its count already read,
        // until released; it reaches the receiver (the log) when it ends.
        if calls.fetch_add(1, Ordering::SeqCst) == 1 {
            let _ = lock(&entered).send(());
            let _ = lock(&release_rx).recv_timeout(LONG);
        }
        lock(&log).push(n);
    }));
    // A re-clean frees the dirty slot: two can run a lease, and that call is held.
    let retrying = std::thread::spawn({
        let book = Arc::clone(&book);
        move || book.retry()
    });
    entered_rx
        .recv_timeout(LONG)
        .expect("the first change reached the hook");
    // Meanwhile the held slot goes dirty: the newest truth is one.
    let (done, done_rx) = mpsc::channel();
    let dropping = std::thread::spawn(move || {
        drop(held);
        let _ = done.send(());
    });
    // With the count and the call under one lock this waits for the first; without, it
    // finishes at once. Either way the release below ends it.
    let _ = done_rx.recv_timeout(Duration::from_millis(500));
    release.send(()).expect("release");
    assert_eq!(retrying.join().expect("retry"), 1);
    dropping.join().expect("drop");
    let last = *lock(&seen).last().expect("a call");
    assert_eq!(
        last,
        book.count(),
        "the receiver's last count is stale: {:?}",
        lock(&seen)
    );
    assert_eq!(last, 1);
}

/// Catches a newly dirty slot that waits out the wait a stubborn one had grown to (the
/// recovery would answer a fresh failure after minutes): with slot 0 never coming back and
/// its wait grown to over a second, slot 1 going dirty is cleaned again within a few
/// first-waits, not after the grown one: the waits asked for are the first, grown while
/// slot 0 stays dirty, and the first again right after slot 1 goes dirty.
#[tokio::test]
async fn a_newly_dirty_slot_restarts_the_backoff() {
    quiet();
    let backoff = Backoff {
        first: Duration::from_millis(10),
        max: Duration::from_secs(3600),
    };
    let cleaner: Cleaner<Stub> = Arc::new(
        |slot: &Stub| {
            if slot.0 == 0 { dirty() } else { Outcome::Clean }
        },
    );
    let (ask, delays) = mpsc::channel();
    let (go, stepping) = mpsc::channel::<()>();
    // As the real wait answers a wake-up: whether a slot went dirty, the signal taken.
    // A test that is done drops `go`: the waits then return at once.
    let nap: Nap = Box::new(move |wake, time| {
        let _ = ask.send(time);
        let _ = stepping.recv();
        std::mem::take(&mut lock(&wake.state).news)
    });
    // Slot 0 is dirty from the start; slot 1 is clean and held.
    let book = Book::start_with(stubs(2), cleaner, backoff, None, nap);
    let (index, permit) = take(&book).await;
    assert_eq!(index, 1);
    let held = book.held(index, permit);
    // The thread begins with the wake-up the start leaves: the first wait ends at once as
    // news (the next starts from the first again), and the passes follow with waits that
    // double.
    let mut asked = Vec::new();
    for _ in 0..5 {
        asked.push(delays.recv_timeout(LONG).expect("a wait"));
        // The fifth stays unanswered: the thread is inside a wait of 80 ms.
        if asked.len() < 5 {
            go.send(()).expect("step");
        }
    }
    // Slot 1 goes dirty during that wait; the wait ends as news.
    drop(held);
    go.send(()).expect("step");
    asked.push(delays.recv_timeout(LONG).expect("a wait"));
    let millis: Vec<u128> = asked.iter().map(Duration::as_millis).collect();
    // Without the restart the last would be the next double of the 80 before it.
    assert_eq!(millis, [10, 10, 20, 40, 80, 10]);
}

/// Catches a recovery thread that cannot be made going unnoticed (the node would sit at
/// its dirty count with nothing saying why): the book still starts and keeps its dirty
/// slot dirty, and the failure is logged (not asserted here, only that nothing panics).
#[test]
fn a_book_whose_recovery_thread_cannot_be_made_still_starts() {
    quiet();
    let cleaner: Cleaner<Stub> = Arc::new(|_: &Stub| dirty());
    let book = Book::start_with(stubs(1), cleaner, FAST, Some(usize::MAX), Box::new(sleep));
    assert_eq!(book.count(), 0);
    assert_eq!(book.dirty(), BTreeSet::from([0]));
}

/// Catches a hook that panics ending the recovery (the thread would die after the first
/// slot it freed, with the other still dirty and nothing to clean it again): the panic is
/// caught, and both slots are freed.
#[test]
fn a_hook_that_panics_does_not_stop_the_recovery() {
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
    let book = Book::start(stubs(2), cleaner, FAST);
    let calls = AtomicUsize::new(0);
    book.on_count(Arc::new(move |_| {
        assert!(calls.fetch_add(1, Ordering::SeqCst) == 0, "the hook panics");
    }));
    gate.store(true, Ordering::SeqCst);
    let give_up = Instant::now() + LONG;
    while book.count() < 2 {
        assert!(Instant::now() < give_up, "the slots never came back");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(book.dirty().is_empty());
}

/// Catches a start that cleans the slots one after the other (the first clean of a slot
/// launches a command as its user, which at a hundred slots would be a hundred in a row),
/// and one that has no bound: with twenty slots whose clean takes a moment, more than one
/// and no more than the workers are in a clean at once, every slot is cleaned once and
/// all are offered.
#[test]
fn the_slots_are_cleaned_at_start_several_at_a_time_and_within_a_bound() {
    quiet();
    let (now, most) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (inside, highest) = (Arc::clone(&now), Arc::clone(&most));
    let cleaner: Cleaner<Stub> = Arc::new(move |_: &Stub| {
        let here = inside.fetch_add(1, Ordering::SeqCst) + 1;
        highest.fetch_max(here, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(30));
        inside.fetch_sub(1, Ordering::SeqCst);
        Outcome::Clean
    });
    let book = Book::start(stubs(20), cleaner, NEVER);
    assert_eq!(book.count(), 20);
    let most = most.load(Ordering::SeqCst);
    assert!((2..=START_WORKERS).contains(&most), "{most} cleans at once");
}
