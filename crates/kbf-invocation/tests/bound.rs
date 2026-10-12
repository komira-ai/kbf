//! The memory bound of docs/design/invocations.md section 3.5, stage 1, and the drop
//! marks of section 3.4 (section 9, PR 3): which record goes first when the records
//! reach the bound, that an operation in flight keeps its record, that only open
//! records left close the largest to new rows within the bound, and that a dropped
//! event marks its record incomplete. Each test names the defect it catches.

mod support;

use kbf_invocation::{
    CallKind, Event, LeaseEnd, Outcome, ROW_CAP, RecordKey, Records, State, WallTime,
};
use kbf_types::OperationId;
use support::*;

/// A GetActionResult hit of `d(action)` by `inv` at `at`: one row with a listing.
/// Every action from 10 to 99 makes a row of the same size.
fn hit(inv: &str, at: WallTime, action: u8) -> Event {
    ev(call(
        inv,
        at,
        CallKind::GetActionResult {
            action: d(action),
            hit: Some(listing(action, 0)),
        },
    ))
}

/// [`hit`] by a server-minted id (section 2.3).
fn minted_hit(inv: &str, at: WallTime, action: u8) -> Event {
    let mut c = call(
        inv,
        at,
        CallKind::GetActionResult {
            action: d(action),
            hit: Some(listing(action, 0)),
        },
    );
    c.minted = true;
    ev(c)
}

/// The bytes the records hold after `events`, with no bound.
fn unbounded(events: &[Event]) -> usize {
    let mut r = Records::new();
    for e in events {
        r.apply(e.clone());
    }
    r.bytes()
}

/// The records after `events` within `bound`. Catches, after every event of every
/// test that uses it, records held over the bound and a total that misses a change
/// to one record or operation (a recount left out).
fn bounded(bound: usize, events: &[Event]) -> Records {
    let mut r = Records::with_bound(ROW_CAP, bound);
    for e in events {
        r.apply(e.clone());
        assert!(
            r.bytes() <= bound,
            "{} bytes held after {e:?}, over the bound of {bound}",
            r.bytes()
        );
        let each: usize = r.keys().map(|k| r.get(k).expect("held").bytes()).sum();
        assert_eq!(
            r.bytes(),
            each + r.operations_bytes(),
            "the total is every record's and operation's bytes, after {e:?}"
        );
    }
    r
}

fn held(r: &Records) -> Vec<String> {
    r.keys().map(ToString::to_string).collect()
}

/// Catches an eviction order other than section 3.5's: a minted record kept while a
/// client's idle record goes (plant "least recently seen first, minted or not": `a`
/// goes before `m`), and idle records taken newest first (plant: `b` before `a`).
#[test]
fn eviction_takes_minted_then_the_least_recently_seen_idle_record() {
    // Three records of one size; the bound holds exactly three.
    let three = [
        hit("a", t(0), 10),
        hit("b", t(10), 11),
        minted_hit("m", t(650), 12),
    ];
    let bound = unbounded(&three);
    let mut r = bounded(bound, &three);
    assert_eq!(held(&r), ["a", "b", "m"]);

    // At t(700) `a` and `b` are idle (quiet for 10 minutes or more); `m`, seen 50 s
    // ago, is not, but minted records go first.
    r.apply(hit("c", t(700), 13));
    assert_eq!(held(&r), ["a", "b", "c"], "the minted record goes first");
    r.apply(hit("d", t(700), 14));
    assert_eq!(
        held(&r),
        ["b", "c", "d"],
        "then the least recently seen idle one"
    );
    r.apply(hit("e", t(700), 15));
    assert_eq!(held(&r), ["c", "d", "e"]);
    assert_eq!(r.evicted(), 3);
    assert!(r.bytes() <= bound);
}

/// Catches an idle test that ignores the operation in flight (plant `state` without
/// `in_flight == 0`: `r`, quiet for 11 minutes but with its operation still running,
/// is evicted instead of `q`).
#[test]
fn an_invocation_whose_operation_still_runs_is_not_idle_and_not_evicted() {
    let events = [
        queued("r", t(0), 10, 1),
        granted(1, 1, "node-a", t(1)),
        started(1, 1, t(2)),
        hit("q", t(5), 11),
        hit("n", t(700), 12),
    ];
    let bound = unbounded(&events) - 1;
    let r = bounded(bound, &events);
    let running = r.get(&key("r")).expect("r is kept");
    assert_eq!(running.state(t(700)), State::Open);
    assert_eq!(held(&r), ["n", "r"], "the idle record `q` goes, `r` stays");
}

/// Catches `last_seen` taken from calls only (plant: drop `inv.touch(at)` in
/// `Records::spread`): `r`'s only operation ran for 11 minutes after its one call and
/// finished at t(690); with no other call `r` would look idle at t(700) and, seen
/// before `q`, be evicted first.
#[test]
fn a_record_whose_operation_ran_past_the_idle_time_is_seen_at_its_finish() {
    let events = [
        queued("r", t(0), 10, 1),
        granted(1, 1, "node-a", t(1)),
        started(1, 1, t(2)),
        hit("q", t(5), 11),
        finished_ok(1, 1, 20, 0, t(690)),
        hit("n", t(700), 12),
    ];
    let bound = unbounded(&events) - 1;
    let r = bounded(bound, &events);
    let ran = r.get(&key("r")).expect("r is kept");
    assert_eq!(ran.last_seen(), t(690), "the finish is r's last event");
    assert_eq!(ran.state(t(700)), State::Open);
    assert_eq!(held(&r), ["n", "r"], "the idle record `q` goes, `r` stays");
}

/// Catches a bound passed when only open records remain (rows taken anyway), a
/// record other than the largest closed (plant "close the smallest": `b` says
/// `rows_capped` after `a`'s overflow), rows refused without `rows_capped`, and
/// actions not counted once rows stop.
#[test]
fn at_the_bound_with_only_open_records_the_largest_stops_taking_rows() {
    let mut events = vec![hit("b", t(0), 10)];
    events.extend((11..21).map(|n| hit("a", t(1), n)));
    let bound = unbounded(&events);
    events.extend((21..41).map(|n| hit("a", t(2), n)));
    let r = bounded(bound, &events);
    let a = r.get(&key("a")).expect("a");
    assert!(a.rows_capped(), "the largest record says rows_capped");
    assert_eq!(a.rows().len(), 10, "no row past the bound");
    assert_eq!(a.counts().ac_hit, 30, "its actions are still counted");
    let b = r.get(&key("b")).expect("b");
    assert!(
        !b.rows_capped(),
        "a smaller record that took no row is not capped"
    );

    // `b`'s next row does not fit either: refused, and `b` says so.
    let mut r = r;
    r.apply(hit("b", t(3), 41));
    assert!(r.bytes() <= bound);
    let b = r.get(&key("b")).expect("b");
    assert!(b.rows_capped());
    assert_eq!(b.rows().len(), 1);
    assert_eq!(r.evicted(), 0, "no open record is removed");
}

/// Catches a collision key reused after the owner's record is evicted (plant the old
/// rule `n = keys held + 1`): the fourth caller, routed once `x` is gone, would get
/// `x~3`, the key of a record still held, and overwrite it.
#[test]
fn a_collision_after_the_owner_is_evicted_gets_a_new_key() {
    let mut second = call(
        "x",
        t(650),
        CallKind::GetActionResult {
            action: d(11),
            hit: Some(listing(11, 0)),
        },
    );
    second.owner = owner("o2");
    let events = [hit("x", t(0), 10), ev(second.clone())];
    // Room for two records and a record with no row (and its index entries): the
    // third, with a row, evicts the idle owner record `x`; the fourth fits.
    let header = unbounded(&[ev(call("y", t(0), CallKind::Downloaded(1)))]);
    let bound = unbounded(&events) + header + 100;
    let mut r = bounded(bound, &events);
    let mut third = second;
    third.owner = owner("o3");
    third.at = t(700);
    r.apply(ev(third));
    assert_eq!(held(&r), ["x~2", "x~3"]);
    let x2 = r
        .get(&RecordKey::collision(id("x"), 2))
        .expect("x~2 is still held");
    assert_eq!(x2.owner(), &owner("o2"), "x~2 is not overwritten");
    // Two records of `x` are held now; the next caller's key is still a new one.
    let mut fourth = call("x", t(700), CallKind::Downloaded(1));
    fourth.owner = owner("o4");
    r.apply(ev(fourth));
    assert_eq!(held(&r), ["x~2", "x~3", "x~4"]);
    let x3 = r
        .get(&RecordKey::collision(id("x"), 3))
        .expect("x~3 is still held");
    assert_eq!(x3.owner(), &owner("o3"), "x~3 is not overwritten");
}

/// Catches an evicted record left among an operation's waiters: a lease event of
/// the minted record's operation after its eviction would look the record up and
/// panic, or re-create it.
#[test]
fn lease_events_of_an_evicted_record_are_ignored() {
    let mut c = call(
        "m",
        t(0),
        CallKind::Execute {
            action: d(10),
            answer: kbf_invocation::ExecuteAnswer::Queued {
                name: name("m", 5),
                operation: OperationId(5),
            },
        },
    );
    c.minted = true;
    // Its grant moves its `last_seen`, so the minted index moves with it.
    let events = [ev(c), granted(5, 1, "node-a", t(1)), hit("a", t(1), 11)];
    let bound = unbounded(&events);
    let mut r = bounded(bound, &events);
    r.apply(hit("b", t(2), 12));
    assert_eq!(
        held(&r),
        ["a", "b"],
        "the minted record goes, in flight or not"
    );
    r.apply(started(5, 1, t(3)));
    r.apply(finished(
        5,
        Some((lease(1), LeaseEnd::Accepted)),
        Outcome::Ok { exit_code: 0 },
        t(4),
    ));
    assert_eq!(held(&r), ["a", "b"]);
}

/// Catches a drop mark that reaches the wrong record (another owner's record of the
/// same id, or a record of another id), and one lost when the dropped call was the
/// record's first (plant "ignore a mark for a record not held": `late` stays
/// complete).
#[test]
fn a_dropped_call_marks_its_own_record_incomplete() {
    let mut squat = call("x", t(1), CallKind::Downloaded(1));
    squat.owner = owner("o2");
    let mut r = Records::new();
    r.apply(ev(call("x", t(0), CallKind::Downloaded(1))));
    r.apply(ev(squat));
    r.apply(ev(call("y", t(0), CallKind::Downloaded(1))));
    r.mark_incomplete_call(&id("x"), &owner(""));
    // The first call of `late` was dropped: its record does not exist yet.
    r.mark_incomplete_call(&id("late"), &owner(""));
    r.apply(ev(call("late", t(2), CallKind::Downloaded(1))));
    let complete = |k: RecordKey| r.get(&k).expect("held").complete();
    assert!(!complete(key("x")));
    assert!(complete(RecordKey::collision(id("x"), 2)));
    assert!(complete(key("y")));
    assert!(
        !complete(key("late")),
        "the record a dropped call would have made"
    );
}

/// Catches a dropped lease event that marks no record, or records that do not wait
/// on its operation.
#[test]
fn a_dropped_lease_event_marks_the_records_waiting_on_its_operation() {
    let mut r = Records::new();
    r.apply(queued("a", t(0), 10, 1));
    r.apply(joined("b", t(0), 10, 1));
    r.apply(queued("c", t(0), 11, 2));
    r.mark_incomplete_operation(OperationId(1));
    let complete = |r: &Records, k: &str| r.get(&key(k)).expect("held").complete();
    assert!(!complete(&r, "a"));
    assert!(!complete(&r, "b"));
    assert!(complete(&r, "c"));
    r.mark_all_incomplete();
    assert!(!complete(&r, "c"));
}

/// A listing of `n` outputs: rows that grow when their operation finishes.
fn wide(result: u8, n: usize) -> kbf_invocation::Listing {
    let mut l = listing(result, 0);
    l.outputs = (0..n)
        .map(|i| kbf_invocation::Output {
            path: format!("out/{i:04}.o"),
            digest: d(result),
            executable: false,
            directory: false,
        })
        .collect();
    l
}

fn finished_wide(op: u64, result: u8, at: WallTime) -> Event {
    Event::Finished(kbf_invocation::Finished {
        operation: OperationId(op),
        lease: None,
        outcome: Outcome::Ok { exit_code: 0 },
        result: Some(wide(result, 50)),
        at,
    })
}

/// Catches a bound held only when a row is added: rows already taken grow when their
/// operation finishes (here by up to 50 outputs each). The idle record goes first
/// (plant "no room made for a listing": `q` stays and the listing is cut at once),
/// then the listing keeps the outputs that fit and its record says
/// `complete: false`; once nothing fits, a new row is refused with `rows_capped`.
#[test]
fn growth_past_the_bound_evicts_idle_records_then_trims_what_fills_in() {
    let mut events = vec![
        hit("q", t(0), 10),
        queued("r", t(700), 11, 1),
        queued("s", t(700), 12, 2),
    ];
    let bound = unbounded(&events);
    events.push(finished_wide(1, 20, t(701)));
    let r = bounded(bound, &events);
    assert_eq!(held(&r), ["r", "s"], "the idle record goes first");
    let ran = &r.get(&key("r")).expect("r").rows()[0];
    let kept = ran.listing.as_ref().expect("listed").outputs.len();
    assert!(kept > 0 && kept < 50, "{kept} of 50 outputs kept");
    assert!(ran.outputs_truncated);
    assert!(!r.get(&key("r")).expect("r").complete());
    assert!(r.get(&key("s")).expect("s").complete());

    events.push(finished_wide(2, 21, t(702)));
    events.push(hit("s", t(703), 13));
    let r = bounded(bound, &events);
    assert_eq!(held(&r), ["r", "s"]);
    let s = r.get(&key("s")).expect("s");
    assert!(!s.complete());
    assert!(s.rows_capped(), "no room for a new row");
    assert_eq!(s.rows().len(), 1);
}

/// Catches an evicted collision left in its owner's list (the owner's page would
/// list a record that is gone).
#[test]
fn an_evicted_collision_leaves_its_owners_list() {
    let mut squat = call("x", t(1), CallKind::Downloaded(1));
    squat.owner = owner("o2");
    let events = [
        ev(call("x", t(0), CallKind::Downloaded(1))),
        ev(squat),
        ev(call("x", t(700), CallKind::Downloaded(1))),
    ];
    let bound = unbounded(&events);
    let mut r = bounded(bound, &events);
    assert_eq!(
        r.get(&key("x")).expect("x").collisions(),
        [RecordKey::collision(id("x"), 2)]
    );
    r.apply(hit("n", t(700), 10));
    assert_eq!(held(&r), ["n", "x"], "the idle collision goes");
    assert!(r.get(&key("x")).expect("x").collisions().is_empty());
}

/// Catches dropped-call marks for records not made yet kept without end: past 1024,
/// the oldest is forgotten (its record starts complete), the newest still marks.
#[test]
fn marks_for_records_not_made_yet_are_capped() {
    let mut r = Records::new();
    for n in 0..1025 {
        r.mark_incomplete_call(&id(&format!("u{n}")), &owner(""));
    }
    r.apply(ev(call("u0", t(0), CallKind::Downloaded(1))));
    r.apply(ev(call("u1024", t(0), CallKind::Downloaded(1))));
    assert!(
        r.get(&key("u0")).expect("u0").complete(),
        "the oldest is forgotten"
    );
    assert!(!r.get(&key("u1024")).expect("u1024").complete());
}
