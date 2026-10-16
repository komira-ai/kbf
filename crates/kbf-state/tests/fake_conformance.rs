//! The in-memory fake against the conformance suite: the correct fake passes every
//! clause, and a fake with one deliberate defect fails the clause that defect breaks.
//! The `expected` text is the start of that clause's assertion message, so each test
//! also pins the reason it fails.

use kbf_state::conformance::{
    c1_linearizable_cas, c2_atomic_txn, c3_monotonic_counter, c4_store_clock_expiry,
    c5_ordered_feed, c6_durable_ack, c7_group_commit, run_all,
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
