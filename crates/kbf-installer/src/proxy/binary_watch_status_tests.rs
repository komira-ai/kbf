//! What the binary watch publishes for the swap-state endpoint ([`WatchStatus`]): the commit of
//! a changed kbf that waits, the refusal of a swap that could not be started, and the numbers
//! of the updates. The bench is the one of the watch's own tests (this module is inside them).

use super::*;

/// The id of a pass that started a swap.
fn started(pass: Pass) -> String {
    match pass {
        Pass::Started(id) => id,
        other => panic!("not a start: {other:?}"),
    }
}

const WHY: &str = "update u1 is Baking";

/// Asserts that a changed kbf that waits for a busy swap is visible, and is gone once the swap
/// starts: the kbf is pending as its commit from the poll that finds the swap busy (not
/// before: the file has not held still), stays so while the swap is busy, and is not pending
/// once the swap is started for it.
#[test]
fn a_changed_kbf_is_pending_while_the_swap_is_busy_and_not_once_it_starts() {
    let mut bench = Bench::new("pend-busy", Some(A), usize::MAX);
    let status = bench.watch.status.clone();
    lock(&bench.swap.0).busy = Some(WHY.to_owned());
    bench.install(B, 1, 0);

    assert_eq!(bench.pass(), Pass::Quiet);
    assert_eq!(status.pending(), None, "the file has not held still");
    for _ in 0..2 {
        assert_eq!(bench.pass(), Pass::Busy(WHY.to_owned()));
        assert_eq!(status.pending(), Some(c(B)));
    }

    lock(&bench.swap.0).busy = None;
    started(bench.pass());

    assert_eq!(status.pending(), None);
    assert_eq!(status.refusal(), None);
}

/// Asserts that the kbf that is the release that serves is not published as a change while the
/// swap is busy (the start-up hold, or a swap resumed to that very release): the file is `A`,
/// `A` serves, and nothing is pending however long the swap stays busy, though the kbf is run
/// once to name its commit.
#[test]
fn a_kbf_that_is_the_release_that_serves_is_not_pending_while_the_swap_is_busy() {
    let mut bench = Bench::new("pend-busy-same", Some(A), usize::MAX);
    let status = bench.watch.status.clone();
    lock(&bench.swap.0).busy = Some(WHY.to_owned());
    bench.install(A, 2, 0);
    let before = bench.calls();

    assert_eq!(bench.pass(), Pass::Quiet);
    for _ in 0..2 {
        assert_eq!(bench.pass(), Pass::Busy(WHY.to_owned()));
        assert_eq!(status.pending(), None);
    }

    assert_eq!(bench.calls(), before + 1, "named once");
}

/// Asserts that a kbf that cannot say its commit is not published as one, and is not run at
/// every poll while the swap is busy: it is pending as nothing, it is run once, and no refusal
/// is made (it names no release to refuse).
#[test]
fn a_kbf_that_names_no_commit_is_pending_as_nothing_and_run_once_while_busy() {
    let mut bench = Bench::new("pend-unknown", Some(A), usize::MAX);
    let status = bench.watch.status.clone();
    lock(&bench.swap.0).busy = Some(WHY.to_owned());
    bench.install("exit 1", 1, 0);
    let before = bench.calls();

    assert_eq!(bench.pass(), Pass::Quiet);
    for _ in 0..3 {
        assert_eq!(bench.pass(), Pass::Busy(WHY.to_owned()));
    }

    assert_eq!(status.pending(), None);
    assert_eq!(bench.calls(), before + 1, "run at more than one poll");
    assert_eq!(status.refusal(), None);
}

/// Asserts that what was published goes with the file it was about: a kbf that is replaced is
/// pending as nothing until the new one holds still and is named (run again, for the new
/// identity), and a kbf that goes is not pending at all.
#[test]
fn a_file_that_changes_or_goes_takes_what_was_published_with_it() {
    let mut bench = Bench::new("pend-change", Some(A), usize::MAX);
    let status = bench.watch.status.clone();
    lock(&bench.swap.0).busy = Some(WHY.to_owned());
    bench.install(B, 1, 0);
    assert_eq!(bench.two_polls(), Pass::Busy(WHY.to_owned()));
    assert_eq!(status.pending(), Some(c(B)));

    bench.install(C, 2, 0);

    assert_eq!(bench.pass(), Pass::Quiet);
    assert_eq!(status.pending(), None, "it was about the old file");
    assert_eq!(bench.pass(), Pass::Busy(WHY.to_owned()));
    assert_eq!(status.pending(), Some(c(C)));

    fs::remove_file(&bench.local.binary).expect("remove the kbf");

    assert_eq!(bench.pass(), Pass::Quiet);
    assert_eq!(status.pending(), None);
}

/// Asserts that a kbf that turns out to be the release that serves is no longer pending: the
/// swap that was busy is over, `B` is what serves, and the file that waited for it is `B`.
#[test]
fn a_kbf_that_turns_out_to_be_the_release_that_serves_is_no_longer_pending() {
    let mut bench = Bench::new("pend-same", Some(A), usize::MAX);
    let status = bench.watch.status.clone();
    lock(&bench.swap.0).busy = Some(WHY.to_owned());
    bench.install(B, 1, 0);
    assert_eq!(bench.two_polls(), Pass::Busy(WHY.to_owned()));
    assert_eq!(status.pending(), Some(c(B)));
    bench.record_serving(B);
    lock(&bench.swap.0).busy = None;

    assert_eq!(bench.pass(), Pass::Same(c(B)));

    assert_eq!(status.pending(), None);
}

/// Asserts that a refused start is published: the refusal is the swap's own text, for the
/// release that was refused, from the release that served, with the id the update would have
/// had, the time of the failure and the first number; the kbf is pending while the failure
/// waits out its backoff, and nothing is published once the swap starts.
#[test]
fn a_refused_start_is_published_with_its_reason_and_the_kbf_stays_pending_until_it_starts() {
    let mut bench = Bench::new("refusal", Some(A), usize::MAX);
    let status = bench.watch.status.clone();
    let refusal = SupervisorError::Refused("update u0 is still open".to_owned());
    lock(&bench.swap.0).refuse = Some(refusal);
    bench.install(B, 1, 0);

    let text = failure(bench.two_polls());

    assert_eq!(text, "swap: refused: update u0 is still open");
    let want = Refusal {
        id: format!("pkg-{}-{START_S}", &B[..12]),
        seq: 1,
        from: Some(c(A)),
        to: c(B),
        reason: text,
        at: START_S,
    };
    assert_eq!(status.refusal(), Some(want.clone()));
    assert_eq!(status.pending(), Some(c(B)));
    assert_eq!(bench.pass(), Pass::Quiet, "waiting out the failure");
    assert_eq!(status.refusal(), Some(want));
    assert_eq!(status.pending(), Some(c(B)));

    lock(&bench.swap.0).refuse = None;
    let (_, pass) = bench.until_next_try();

    started(pass);
    assert_eq!(status.pending(), None);
    assert_eq!(status.refusal(), None);
}

/// Asserts that a kbf refused again on each retry is shown as the same update: the id, the
/// number and the time stay those of the first failure while the reason is the newest, and the
/// refusal of another release takes the next number.
#[test]
fn a_kbf_refused_again_on_a_retry_is_the_same_refusal_with_the_newest_reason() {
    let mut bench = Bench::new("refusal-retry", Some(A), usize::MAX);
    let status = bench.watch.status.clone();
    let first = SupervisorError::Refused("update u0 is still open".to_owned());
    lock(&bench.swap.0).refuse = Some(first);
    bench.install(B, 1, 0);
    failure(bench.two_polls());
    let before = status.refusal().expect("a refusal");

    let second = SupervisorError::Refused("update u1 is still open".to_owned());
    lock(&bench.swap.0).refuse = Some(second);
    let (_, pass) = bench.until_next_try();
    let reason = failure(pass);

    let after = status.refusal().expect("a refusal");
    assert_eq!(reason, "swap: refused: update u1 is still open");
    assert_eq!(after.reason, reason);
    assert_ne!(after.reason, before.reason);
    assert_eq!(after.id, before.id);
    assert_eq!(after.seq, before.seq);
    assert_eq!(after.at, START_S);

    status.refuse("pkg-other".to_owned(), None, c(C), "again".to_owned(), 9);

    let other = status.refusal().expect("a refusal");
    assert_eq!(other.seq, before.seq + 1);
    assert_eq!(other.id, "pkg-other");
    assert_eq!(other.at, 9);
}

/// Asserts that the endpoint stops showing what came before once a swap is running: a kbf
/// refused once is started on its retry, and by the time the copies are pruned (after the swap
/// has started) the refusal is gone and nothing is pending.
#[test]
fn nothing_stands_refused_or_pending_once_the_swap_started_even_while_the_copies_are_pruned() {
    let mut bench = Bench::new("clear-early", Some(A), usize::MAX);
    let status = bench.watch.status.clone();
    let refusal = SupervisorError::Refused("update u0 is still open".to_owned());
    lock(&bench.swap.0).refuse = Some(refusal);
    bench.install(B, 1, 0);
    failure(bench.two_polls());
    assert_eq!(status.pending(), Some(c(B)));

    let refusals = Arc::new(Mutex::new(Vec::new()));
    let pendings = Arc::new(Mutex::new(Vec::new()));
    let (into_refusals, into_pendings) = (refusals.clone(), pendings.clone());
    let watched = status.clone();
    bench.watch.in_use = Box::new(move || {
        lock(&into_refusals).push(watched.refusal());
        lock(&into_pendings).push(watched.pending());
        Vec::new()
    });
    lock(&bench.swap.0).refuse = None;

    started(bench.until_next_try().1);

    assert_eq!(*lock(&refusals), vec![None]);
    assert_eq!(*lock(&pendings), vec![None]);
}

/// Asserts that a refusal names a serving release only when there is one, and that a failure
/// that names no release is not a refusal: with no installed release recorded, or a record
/// that cannot be read, the refusal has no `from`; a kbf that fails to run is refused as
/// nothing and pends as nothing.
#[test]
fn a_refusal_names_the_serving_release_only_when_there_is_one() {
    let mut bench = Bench::new("refusal-unserved", None, usize::MAX);
    let status = bench.watch.status.clone();
    bench.install(B, 1, 0);
    let text = failure(bench.two_polls());
    assert_eq!(text, "no installed release is recorded");
    let refusal = status.refusal().expect("a refusal");
    assert_eq!(refusal.from, None);
    assert_eq!(refusal.to, c(B));
    assert_eq!(refusal.reason, text);

    let mut bench = Bench::new("refusal-torn", Some(A), usize::MAX);
    let status = bench.watch.status.clone();
    fs::write(bench.status_file(), b"{").expect("torn releases.json");
    bench.install(B, 1, 0);
    let text = failure(bench.two_polls());
    assert!(text.starts_with("status: "), "{text}");
    let refusal = status.refusal().expect("a refusal");
    assert_eq!(refusal.from, None, "the record cannot be read");
    assert_eq!(refusal.reason, text);

    let mut bench = Bench::new("refusal-probe", Some(A), usize::MAX);
    let status = bench.watch.status.clone();
    bench.install("exit 1", 1, 0);
    assert!(matches!(bench.two_polls(), Pass::Failed(_)));
    assert_eq!(status.refusal(), None);
    assert_eq!(status.pending(), None);
}

/// Asserts that updates are counted once each: the swaps the watch starts are numbered 1 and 2
/// in the order they start (the watch asks for the number of the swap it starts), asking for a
/// number again gives the same one, a refusal takes the next, and an id asked about for the
/// first time (an update the proxy resumed) takes the one after.
#[test]
fn updates_are_numbered_in_the_order_they_start_and_a_number_is_kept() {
    let mut bench = Bench::new("seq", Some(A), usize::MAX);
    let status = bench.watch.status.clone();
    bench.install(B, 1, 0);
    let first = started(bench.two_polls());
    bench.install(C, 2, 0);
    let second = started(bench.two_polls());
    let refusal = SupervisorError::Refused("update u0 is still open".to_owned());
    lock(&bench.swap.0).refuse = Some(refusal);
    bench.install(D, 3, 0);
    failure(bench.two_polls());

    assert_eq!(status.seq_of(&second), 2);
    assert_eq!(status.seq_of(&first), 1);
    assert_eq!(status.seq_of(&first), 1, "a number is kept");
    assert_eq!(status.refusal().map(|r| r.seq), Some(3));
    assert_eq!(status.seq_of("pkg-resumed"), 4);
    assert_eq!(status.seq_of("pkg-resumed"), 4);
}

/// Asserts that a watch publishes to the status it is given, not one of its own: the given one
/// is the one that shows what waits.
#[test]
fn a_watch_publishes_to_the_status_it_is_given() {
    let mut bench = Bench::new("given", Some(A), usize::MAX);
    let own = bench.watch.status.clone();
    let given = WatchStatus::default();
    let kept: InUse = Box::new(Vec::new);
    let swap = Box::new(bench.swap.clone());
    let watch = Watch::new(bench.local.clone(), swap, bench.clock.clone(), kept);
    bench.watch = watch.publishing(given.clone());
    lock(&bench.swap.0).busy = Some(WHY.to_owned());
    bench.install(B, 1, 0);

    bench.two_polls();

    assert_eq!(given.pending(), Some(c(B)));
    assert_eq!(own.pending(), None);
}
