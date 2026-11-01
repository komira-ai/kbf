//! What a frontend logs and counts for each way the state store can refuse a committed
//! record's write. A refusal as stale is the loss of a race with another frontend for the
//! operation: one WARN line naming the operation and a lost race, not a dropped write.
//! Every other refusal keeps its ERROR lines and its count: a store that cannot be reached
//! (unavailable, timed out, an I/O error) holds the record back and counts nothing; a
//! fence, a node another frontend owns, an unencodable write and the held cap each drop
//! the write and count it in `dropped_writes`. A refusal as stale of another write than a
//! committed record's is logged as before.

use kbf_state::StoreError;

use super::blocks_tests::{Racing, farm_over, submit};
use super::intake_tests::{caller, invalid, register};
use super::*;
use crate::test_log::capture;

/// The line of a lost race.
const RACE: &str = "another frontend won the race for the operation";

/// What one refusal of a committed record left behind.
struct Refused {
    log: String,
    operation: OperationId,
    lost_races: u64,
    dropped_writes: u64,
    /// The farm's `/metrics` lines after the refusal.
    metrics: String,
}

impl Refused {
    fn lines(&self, level: &str) -> Vec<&str> {
        self.log.lines().filter(|l| l.contains(level)).collect()
    }
}

/// A farm over a store that places one operation on a node, then refuses the write of its
/// result's committed record with `error`.
async fn refused_with(error: StateError) -> Refused {
    let store = Racing::new();
    let farm = farm_over(&store);
    let node = WorkerId::new("node-a");
    let _inbox = register(&farm, "node-a");
    let _ticket = caller(&farm, 1);
    let views = farm.lease_views(None);
    let [view] = &views.leases[..] else {
        panic!("one lease expected");
    };
    let (lease, operation) = (view.lease, view.operation);
    let durable = |farm: &Farm| {
        let state = farm.lock();
        let durable = state.engine.durable.as_ref().expect("a state store");
        durable.refuse_next("a committed record", error);
    };
    durable(&farm);

    let log = capture();
    farm.report(&node, invalid(lease)).await;

    let mut metrics = String::new();
    crate::pack_daemon_metrics::render_farm(&mut metrics, &farm);
    Refused {
        log: log.text(),
        operation,
        lost_races: farm.lost_races(),
        dropped_writes: farm.dropped_writes(),
        metrics,
    }
}

/// Asserts that `refused` logged the ERROR lines of a dropped committed record (the store
/// did not take it; its remaining effects were dropped), counted one dropped write and no
/// lost race, and wrote no race line.
fn assert_dropped(refused: &Refused, why: &str) {
    let errors = refused.lines("ERROR");
    assert_eq!(errors.len(), 2, "{why}: {}", refused.log);
    assert!(
        errors[0].contains("the state store did not take a committed record"),
        "{why}: {}",
        refused.log
    );
    assert!(
        errors[1].contains("a committed record's remaining effects were dropped"),
        "{why}: {}",
        refused.log
    );
    assert!(!refused.log.contains(RACE), "{why}: {}", refused.log);
    assert_eq!(
        (refused.dropped_writes, refused.lost_races),
        (1, 0),
        "{why}"
    );
}

/// Asserts that a committed record the store refuses as stale (a row the write depends on
/// is gone, or changed under it) is one WARN line naming the operation and one lost race,
/// with no ERROR line and no dropped write, and that `/metrics` shows the race in
/// `kbf_farm_lost_races_total`. Catches: the race logged at ERROR, logged twice, named
/// without its operation, counted as a dropped write, or missing from the exposition.
#[tokio::test(start_paused = true)]
async fn a_stale_refusal_is_one_warning_and_a_lost_race() {
    let stale = [
        StateError::Missing("lease row".to_owned()),
        StateError::Conflict(Conflict {
            index: 0,
            key: "lease/".to_owned(),
        }),
    ];
    for error in stale {
        let why = error.to_string();
        let refused = refused_with(error).await;

        assert!(refused.lines("ERROR").is_empty(), "{why}: {}", refused.log);
        let races = refused.lines(RACE);
        let [warning] = &races[..] else {
            panic!("{why}: one race line expected: {}", refused.log);
        };
        assert!(warning.contains(" WARN "), "{why}: {warning}");
        let named = format!("operations={}", refused.operation);
        assert!(warning.contains(&named), "{why}: {warning}");
        assert_eq!(
            (refused.lost_races, refused.dropped_writes),
            (1, 0),
            "{why}"
        );
        let exposed = |line: &str| refused.metrics.lines().any(|l| l == line);
        assert!(
            exposed("kbf_farm_lost_races_total 1"),
            "{}",
            refused.metrics
        );
        assert!(
            exposed("kbf_farm_dropped_writes_total 0"),
            "{}",
            refused.metrics
        );
    }
}

/// Asserts that a store that cannot be reached (it is unavailable, a call timed out, an I/O
/// error) keeps the ERROR line of a write it did not take, holds the record back to be
/// written again, and counts neither a dropped write nor a lost race. Catches: an outage
/// taken for a lost race, or its ERROR line lost.
#[tokio::test(start_paused = true)]
async fn an_unreachable_store_keeps_its_error_and_holds_the_record() {
    let causes = [
        "the store is unavailable",
        "the call timed out",
        "I/O error: connection reset",
    ];
    for cause in causes {
        let refused = refused_with(StateError::Store(StoreError::Failed(cause.to_owned()))).await;

        let errors = refused.lines("ERROR");
        let [error] = &errors[..] else {
            panic!("{cause}: one ERROR line expected: {}", refused.log);
        };
        assert!(
            error.contains("the state store did not take a committed record"),
            "{cause}: {error}"
        );
        assert!(error.contains(cause), "{cause}: {error}");
        assert!(
            refused.log.contains("retried later"),
            "{cause}: {}",
            refused.log
        );
        assert!(!refused.log.contains(RACE), "{cause}: {}", refused.log);
        assert_eq!(
            (refused.dropped_writes, refused.lost_races),
            (0, 0),
            "{cause}"
        );
    }
}

/// Asserts that a write refused because a newer writer fenced the store keeps both ERROR
/// lines and counts one dropped write, no lost race.
#[tokio::test(start_paused = true)]
async fn a_fenced_refusal_keeps_its_errors_and_counts_a_drop() {
    let refused = refused_with(StateError::Store(StoreError::Fenced)).await;
    assert_dropped(&refused, "fenced");
}

/// Asserts that a write refused because the node belongs to another frontend now keeps
/// both ERROR lines and counts one dropped write, no lost race.
#[tokio::test(start_paused = true)]
async fn a_refusal_for_a_node_another_frontend_owns_keeps_its_errors_and_counts_a_drop() {
    let refused = refused_with(StateError::StaleOwner(WorkerId::new("node-a"))).await;
    assert_dropped(&refused, "a foreign node");
}

/// Asserts that a write that cannot be encoded (or whose row cannot be read) keeps both
/// ERROR lines and counts one dropped write, no lost race.
#[tokio::test(start_paused = true)]
async fn an_unencodable_write_keeps_its_errors_and_counts_a_drop() {
    let refused = refused_with(StateError::Encode(kbf_log::EncodeError::Record)).await;
    assert_dropped(&refused, "unencodable");
    let corrupt = StateError::Corrupt {
        key: "op/".to_owned(),
        why: "damaged".to_owned(),
    };
    let refused = refused_with(corrupt).await;
    assert_dropped(&refused, "corrupt");
}

/// Asserts that a record dropped over the held cap keeps its ERROR line and counts one
/// dropped write, no lost race.
#[tokio::test(start_paused = true)]
async fn a_record_over_the_held_cap_keeps_its_error_and_counts_a_drop() {
    let store = Racing::new();
    let farm = farm_over(&store).with_held_cap(1);
    let node = WorkerId::new("node-a");
    let _inbox = register(&farm, "node-a");
    let _tickets = (caller(&farm, 1), caller(&farm, 2));
    let views = farm.lease_views(None);
    let [first, second] = &views.leases[..] else {
        panic!("two leases expected");
    };
    store.set_down(true);
    farm.report(&node, invalid(first.lease)).await;

    let log = capture();
    farm.report(&node, invalid(second.lease)).await;
    let log = log.text();

    assert!(log.contains("ERROR"), "{log}");
    assert!(log.contains("records over the cap: one dropped"), "{log}");
    assert!(!log.contains(RACE), "{log}");
    assert_eq!((farm.dropped_writes(), farm.lost_races()), (1, 0));
}

/// Asserts that a refusal as stale of a write that is not a committed record's (here, the
/// queue rows of a submission) is logged at ERROR as before, and counts neither a lost race
/// nor a dropped write. Catches: the race treatment reaching the other writers.
#[tokio::test(start_paused = true)]
async fn a_stale_refusal_of_another_write_is_logged_as_before() {
    let store = Racing::new();
    let farm = farm_over(&store);
    {
        let state = farm.lock();
        let durable = state.engine.durable.as_ref().expect("a state store");
        let stale = StateError::Missing("queue row".to_owned());
        durable.refuse_next("a submission", stale);
    }

    let log = capture();
    let submitted = submit(&farm, 1);
    let log = log.text();

    assert!(submitted.is_err(), "a submission the store did not take");
    let errors: Vec<_> = log.lines().filter(|l| l.contains("ERROR")).collect();
    let [error] = &errors[..] else {
        panic!("one ERROR line expected: {log}");
    };
    assert!(
        error.contains("the state store did not take a submission"),
        "{error}"
    );
    assert!(error.contains("the write is stale"), "{error}");
    assert!(!log.contains(RACE), "{log}");
    assert_eq!((farm.lost_races(), farm.dropped_writes()), (0, 0));
}
