//! What a frontend logs and counts for each way the state store can refuse a committed
//! record's write. A refusal as stale is the loss of a race with another frontend for the
//! operation: one WARN line naming the operation and a lost race, not a dropped write.
//! Every other refusal keeps its ERROR lines and its count: a store that cannot be reached
//! (unavailable, timed out, an I/O error) holds the record back and counts nothing; a
//! fence, a node another frontend owns, an unencodable write and the held cap each drop
//! the write and count it in `dropped_writes`. A refusal as stale of another write than a
//! committed record's is logged as before. A record held back by an unreachable store that
//! loses the race before it is written again is one lost race, once, on the retry.

use kbf_state::StoreError;

use super::blocks_tests::{Racing, durable_over, farm_over, submit, unfinished};
use super::intake_tests::{caller, invalid, register};
use super::owner_tests::grant;
use super::transition_tests::{answer, start};
use super::*;
use crate::test_log::capture;

/// The start of the line of a lost race.
const RACE: &str = "another frontend won the race for the operation";

/// The whole message of a lost race.
const LOST_RACE: &str = "another frontend won the race for the operation: this frontend's \
                         write is dropped and its placement undone";

/// The lost races `farm` counted: the legacy core, which every test here runs, counts them.
fn races(farm: &Farm) -> u64 {
    farm.lost_races()
        .expect("the legacy core counts lost races")
}

/// The farm's `/metrics` page, as `GET /metrics` writes it.
fn scrape(farm: &Farm) -> String {
    let mut text = String::new();
    crate::metrics::render_farm(&mut text, farm);
    text
}

/// Lets `farm` notice that its state store is down and still refuses the held records, so
/// the log of the next tick holds what that tick writes and not the outage's notice: one
/// tick with the store down (it says the store is unavailable, and the held records are
/// refused again), and the store comes back.
async fn notice_the_outage(store: &Racing, farm: &Farm) {
    store.set_down(true);
    farm.tick().await;
    store.set_down(false);
}

/// Refuses the next write named `what` on `farm`'s state store with `error`.
fn refuse_next(farm: &Farm, what: &'static str, error: StateError) {
    let state = farm.lock();
    let durable = state.engine.durable.as_ref().expect("a state store");
    durable.refuse_next(what, error);
}

/// Asserts that `log` holds exactly one WARN line, the whole message of a lost race with
/// `operations` set to exactly `named` and nothing after it, and no ERROR line.
fn assert_one_race_warning(log: &str, named: &str, why: &str) {
    let warnings: Vec<_> = log.lines().filter(|l| l.contains(" WARN ")).collect();
    let [warning] = &warnings[..] else {
        panic!("{why}: one WARN line expected: {log}");
    };
    let wanted = format!(": {LOST_RACE} operations={named}");
    assert!(warning.ends_with(&wanted), "{why}: {warning}");
    assert!(!log.contains(" ERROR "), "{why}: {log}");
}

/// What one refusal of a committed record left behind.
struct Refused {
    /// The store under the farm.
    store: Arc<Racing>,
    /// The farm the record was refused on.
    farm: Farm,
    log: String,
    operation: OperationId,
    lost_races: u64,
    dropped_writes: u64,
    /// The committed records held back, unwritten, after the refusal.
    held: usize,
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
    refuse_next(&farm, "a committed record", error);

    let log = capture();
    farm.report(&node, invalid(lease)).await;

    let held = farm.lock().engine.unwritten.len();
    Refused {
        log: log.text(),
        operation,
        lost_races: races(&farm),
        dropped_writes: farm.dropped_writes(),
        held,
        farm,
        store,
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
/// is gone, or changed under it) is exactly one WARN line, the whole race message with
/// `operations` naming the operation, and one lost race, with no ERROR line and no dropped
/// write, and that `/metrics` shows the race in `kbf_farm_lost_races_total`. The result is
/// held back by an unreachable store first, whose outage the farm notices, so the log of
/// the retry (a tick) holds only the store's return (INFO) and what the refusal wrote. Catches: the race logged at ERROR, logged twice or beside another
/// WARN, its message cut short, its operation missing or wrong, counted as a dropped write,
/// or missing from the exposition.
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
        let held = StateError::Store(StoreError::Failed("unavailable".to_owned()));
        let refused = refused_with(held).await;
        assert_eq!(refused.held, 1, "{why}: the result was not held back");
        let farm = refused.farm;
        notice_the_outage(&refused.store, &farm).await;
        assert_eq!(farm.lock().engine.unwritten.len(), 1, "{why}: not held");
        refuse_next(&farm, "a committed record", error);

        let log = capture();
        farm.tick().await;
        let log = log.text();

        assert_one_race_warning(&log, &refused.operation.to_string(), &why);
        assert_eq!((races(&farm), farm.dropped_writes()), (1, 0), "{why}");
        let metrics = scrape(&farm);
        let exposed = |line: &str| metrics.lines().any(|l| l == line);
        assert!(exposed("kbf_farm_lost_races_total 1"), "{metrics}");
        assert!(exposed("kbf_farm_dropped_writes_total 0"), "{metrics}");
    }
}

/// Asserts that a grant held back by an unreachable store, whose operation another frontend
/// takes before the grant is written again, is one lost race on the retry: exactly one WARN
/// line naming the operation, no ERROR, `(lost_races, dropped_writes) == (1, 0)`, and the
/// placement undone (this frontend holds no lease); and that the ticks and reports after it
/// neither warn nor count the race again. Catches: a race counted at the outage instead of
/// at the refusal, counted or warned again on a later retry, or its placement kept.
#[tokio::test(start_paused = true)]
async fn a_held_grant_that_loses_the_race_on_its_retry_is_one_lost_race() {
    let store = Racing::new();
    let farm = farm_over(&store);
    let _ticket = caller(&farm, 1);
    let [operation] = unfinished(&farm)[..] else {
        panic!("one operation expected");
    };
    let unavailable = StateError::Store(StoreError::Failed("unavailable".to_owned()));
    refuse_next(&farm, "a committed record", unavailable);
    let _inbox = register(&farm, "node-a");
    assert_eq!(
        farm.lock().engine.unwritten.len(),
        1,
        "the grant was not held"
    );
    let views = farm.lease_views(None);
    let [view] = &views.leases[..] else {
        panic!("one placement expected");
    };
    let lease = view.lease;
    assert_eq!(
        (races(&farm), farm.dropped_writes()),
        (0, 0),
        "at the outage"
    );
    notice_the_outage(&store, &farm).await;
    let other = durable_over(&store);
    grant(&other, operation.0, 0, "node-b").expect("granted by another frontend");

    let log = capture();
    farm.tick().await;
    let log = log.text();

    assert_one_race_warning(&log, &operation.to_string(), "the retry");
    assert_eq!((races(&farm), farm.dropped_writes()), (1, 0));
    assert!(
        farm.lease_views(None).leases.is_empty(),
        "the placement was kept"
    );
    assert!(
        farm.lock().engine.unwritten.is_empty(),
        "the grant is still held"
    );

    let later = capture();
    for _ in 0..3 {
        tokio::time::advance(Duration::from_secs(5)).await;
        farm.tick().await;
    }
    farm.report(&WorkerId::new("node-a"), invalid(lease)).await;
    let later = later.text();

    assert!(!later.contains(RACE), "the race was logged again: {later}");
    assert!(!later.contains(" ERROR "), "{later}");
    assert_eq!(
        (races(&farm), farm.dropped_writes()),
        (1, 0),
        "after the retry"
    );
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
        assert_eq!(refused.held, 1, "{cause}: the record was not held back");
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
    assert_eq!((farm.dropped_writes(), races(&farm)), (1, 0));
}

/// Asserts that a refusal as stale of a write that is not a committed record's (here, the
/// queue rows of a submission) is logged at ERROR as before, and counts neither a lost race
/// nor a dropped write. Catches: the race treatment reaching the other writers.
#[tokio::test(start_paused = true)]
async fn a_stale_refusal_of_another_write_is_logged_as_before() {
    let store = Racing::new();
    let farm = farm_over(&store);
    let stale = StateError::Missing("queue row".to_owned());
    refuse_next(&farm, "a submission", stale);

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
    assert_eq!((races(&farm), farm.dropped_writes()), (0, 0));
}

/// Asserts that the operations a lost race names are those of every effect that has one
/// (a start, an answer, a refusal, a wait), each once however often the effects repeat it,
/// in the order first named, and that a committed record names none. Catches a log line
/// that names nothing or the same operation twice.
#[test]
fn the_operations_a_lost_race_names_are_those_of_its_effects() {
    let grant = LeaseGrant {
        lease: LeaseId::new(5, 0),
        operation: OperationId(9),
        worker: WorkerId::new("node-a"),
        cost: Default::default(),
    };
    let refusal = kbf_types::Refusal {
        operation: OperationId(4),
        waiters: Vec::new(),
        reason: String::new(),
    };
    let effects = VecDeque::from([
        Effect::Commit(ControlRecord::Lease(grant)),
        Effect::Start(start(7, 0, "node-a")),
        Effect::Waiting(kbf_types::Waiting {
            operation: OperationId(3),
            reason: None,
        }),
        Effect::Answer(answer(3, 1)),
        Effect::Refuse(refusal),
        Effect::Start(start(7, 1, "node-a")),
    ]);

    assert_eq!(operations_of(&effects), "op-7,op-3,op-4");
    assert_eq!(operations_of(&VecDeque::new()), "");
}
