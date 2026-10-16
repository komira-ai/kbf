//! The in-memory fake against the conformance suite: the correct fake passes every
//! clause, and a fake with one deliberate defect fails the clause that defect breaks.
//! The `expected` text is the start of that clause's assertion message, so each test
//! also pins the reason it fails.

use kbf_state::conformance::{
    c1_linearizable_cas, c2_atomic_txn, c3_monotonic_counter, c4_store_clock_expiry,
    c5_ordered_feed, c6_durable_ack, c7_group_commit, c8_bounded_range_scan, run_all,
};
use kbf_state::fake::{FakeBackend, Flaw};

#[test]
fn the_correct_fake_passes_every_clause() {
    run_all(&FakeBackend::new(Flaw::None));
}

// Catches a compare-and-set that checks and writes in separate critical sections.
#[test]
#[should_panic(expected = "C1 race")]
fn non_linearizable_cas_fails_c1() {
    c1_linearizable_cas(&FakeBackend::new(Flaw::NonLinearizableCas));
}

// Catches a transaction that applies operations until one fails.
#[test]
#[should_panic(expected = "C2 aborted")]
fn partial_transaction_fails_c2() {
    c2_atomic_txn(&FakeBackend::new(Flaw::PartialTxn));
}

// Catches a counter that is not durable, so a value is reissued after restart.
#[test]
#[should_panic(expected = "C3 restart")]
fn forgotten_counter_fails_c3() {
    c3_monotonic_counter(&FakeBackend::new(Flaw::CounterNotDurable));
}

// Catches expiry judged by a clock that is not the store's current one.
#[test]
#[should_panic(expected = "C4 expiry")]
fn caller_clock_expiry_fails_c4() {
    c4_store_clock_expiry(&FakeBackend::new(Flaw::CallerClockExpiry));
}

// Catches a feed that advances past a revision that has not committed: the poller
// that saw the later revision first never sees the earlier one.
#[test]
#[should_panic(expected = "C5 interleaved")]
fn feed_past_an_uncommitted_revision_fails_c5() {
    c5_ordered_feed(&FakeBackend::new(Flaw::FeedPastPending));
}

// Catches an acknowledgement sent before the data is durable.
#[test]
#[should_panic(expected = "C6 durable")]
fn ack_before_sync_fails_c6() {
    c6_durable_ack(&FakeBackend::new(Flaw::AckBeforeSync));
}

// Catches writes that pay a sync each, so a burst misses the commit window.
#[test]
#[should_panic(expected = "C7 latency")]
fn ungrouped_writes_fail_c7() {
    c7_group_commit(&FakeBackend::new(Flaw::Ungrouped));
}

// Catches a range scan that returns every key of its range, past its limit.
#[test]
#[should_panic(expected = "C8 limit")]
fn a_scan_that_ignores_its_limit_fails_c8() {
    c8_bounded_range_scan(&FakeBackend::new(Flaw::ScanIgnoresLimit));
}

// Catches a range scan that begins at the prefix whatever lower bound it is given.
#[test]
#[should_panic(expected = "C8 lower")]
fn a_scan_that_ignores_its_lower_bound_fails_c8() {
    c8_bounded_range_scan(&FakeBackend::new(Flaw::ScanIgnoresLower));
}

// Catches a resume token that resumes at the last key of its page instead of after it,
// so the key is returned twice.
#[test]
#[should_panic(expected = "C8 resume")]
fn a_scan_that_resumes_at_its_token_fails_c8() {
    c8_bounded_range_scan(&FakeBackend::new(Flaw::ScanResumesAtToken));
}

// Catches a range scan that runs past the last key under its prefix.
#[test]
#[should_panic(expected = "C8 prefix")]
fn a_scan_past_its_prefix_fails_c8() {
    c8_bounded_range_scan(&FakeBackend::new(Flaw::ScanPastPrefix));
}

// Catches a forward scan that runs on past its upper bound.
#[test]
#[should_panic(expected = "C8 upper: an included upper key is the last row")]
fn a_scan_that_ignores_its_upper_bound_fails_c8() {
    c8_bounded_range_scan(&FakeBackend::new(Flaw::ScanIgnoresUpper));
}

// Catches a forward scan that returns the key of an excluded upper bound.
#[test]
#[should_panic(expected = "C8 upper: an excluded upper key is not returned")]
fn a_scan_that_includes_an_excluded_upper_key_fails_c8() {
    c8_bounded_range_scan(&FakeBackend::new(Flaw::ScanIncludesExcludedUpper));
}

// Catches a reverse scan built as the forward page reversed: the first keys of the
// range, not the last (`ORDER BY key LIMIT n` then reversing the rows).
#[test]
#[should_panic(expected = "C8 reverse: a reverse page holds the last keys")]
fn a_reverse_scan_of_the_forward_page_fails_c8() {
    c8_bounded_range_scan(&FakeBackend::new(Flaw::ScanReversesForwardPage));
}

// Catches a reverse scan whose token resumes at the last key of its page, so the key is
// returned twice.
#[test]
#[should_panic(expected = "C8 reverse resume")]
fn a_reverse_scan_that_resumes_at_its_token_fails_c8() {
    c8_bounded_range_scan(&FakeBackend::new(Flaw::ScanReverseResumesAtToken));
}

// The two error types say what failed.
#[test]
fn errors_say_what_failed() {
    let conflict = kbf_state::Conflict {
        index: 2,
        key: "k".to_owned(),
    };
    assert_eq!(
        conflict.to_string(),
        "operation 2 on \"k\" failed its precondition"
    );
    let failure = kbf_state::StoreError::Failed("disk full".to_owned());
    assert_eq!(failure.to_string(), "state store failure: disk full");
}
