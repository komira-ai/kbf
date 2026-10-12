//! The memory bound of docs/design/invocations.md section 3.5, stage 1, held at all
//! times (section 9, PR 3): rows already taken fill in (attempts, an outcome, a
//! listing) after their record stops taking rows, records and the operations their
//! calls wait on are made, and headers learn new instance names. Each of these is
//! admitted against the bound like a new row; what does not fit is left out and the
//! record says `complete: false` (a listing keeps the outputs that fit and says
//! `outputs_truncated`). Every test checks the bound after every event it applies.
//! Each test names the defect it catches.

mod support;

use kbf_invocation::{
    CallKind, Event, ExecuteAnswer, Finished, LeaseEnd, Outcome, Output, ROW_CAP, Records, WallTime,
};
use kbf_types::{Failure, OperationId};
use support::*;

/// Applies `e`, then checks the records are within their bound and that the total
/// is a fresh count: every record's bytes and every operation's.
fn step(r: &mut Records, e: Event) {
    let shown = format!("{e:?}");
    let shown = &shown[..shown.floor_char_boundary(200)];
    r.apply(e);
    assert!(
        r.bytes() <= r.bound(),
        "{} bytes held after {shown}, over the bound of {}",
        r.bytes(),
        r.bound()
    );
    let each: usize = r.keys().map(|k| r.get(k).expect("held").bytes()).sum();
    assert_eq!(
        r.bytes(),
        each + r.operations_bytes(),
        "the total is every record's and operation's bytes, after {shown}"
    );
}

/// The records after `events`, with no bound.
fn unbounded(row_cap: usize, events: &[Event]) -> Records {
    let mut r = Records::with_row_cap(row_cap);
    for e in events {
        r.apply(e.clone());
    }
    r
}

/// The records after `events` within `bound`, checked after each.
fn bounded(row_cap: usize, bound: usize, events: &[Event]) -> Records {
    let mut r = Records::with_bound(row_cap, bound);
    for e in events {
        step(&mut r, e.clone());
    }
    r
}

/// `n` outputs with buck2-shaped paths.
fn outputs(n: usize) -> Vec<Output> {
    (0..n)
        .map(|k| Output {
            path: format!("buck-out/v2/gen/root/0123456789abcdef/some/pkg/__lib__/{k:05}.o"),
            digest: d(30),
            executable: false,
            directory: false,
        })
        .collect()
}

/// `op` finished OK, its result listing `n` outputs.
fn finished_with(op: u64, n: usize, at: WallTime) -> Event {
    let mut l = listing(30, 0);
    l.outputs = outputs(n);
    Event::Finished(Finished {
        operation: OperationId(op),
        lease: None,
        outcome: Outcome::Ok { exit_code: 0 },
        result: Some(l),
        at,
    })
}

fn complete(r: &Records, inv: &str) -> bool {
    r.get(&key(inv)).expect("held").complete()
}

/// Catches listings filled in past the bound (the verifier's probe: 20 open rows
/// each finishing with 1,000 outputs held 184 times the bound). Plant "set the whole
/// listing" in the finish: the first finish goes over. The rows keep the outputs
/// that fit, say `outputs_truncated`, and their record says `complete: false`.
#[test]
fn open_rows_that_fill_in_stay_within_the_bound() {
    let calls: Vec<Event> = (0..20u64)
        .map(|i| queued("r", t(10), 10 + u8::try_from(i).expect("small"), i))
        .collect();
    let bound = unbounded(ROW_CAP, &calls).bytes() + 10_000;
    let mut r = bounded(ROW_CAP, bound, &calls);
    for i in 0..20 {
        step(&mut r, finished_with(i, 1000, t(11)));
    }
    let inv = r.get(&key("r")).expect("r");
    assert_eq!(inv.rows().len(), 20, "every row taken stays");
    let kept: usize = inv
        .rows()
        .iter()
        .map(|row| row.listing.as_ref().map_or(0, |l| l.outputs.len()))
        .sum();
    assert!(kept > 0, "the outputs that fit are kept");
    assert!(kept < 20_000, "not every output fits");
    assert!(inv.rows().iter().all(|row| row.outputs_truncated));
    assert!(!inv.complete(), "the record says what it left out");
    assert!(
        inv.rows().iter().all(|row| row.outcome.is_some()),
        "every outcome is kept"
    );
}

/// Catches attempts copied into rows past the bound (plant "copy every attempt" in
/// the spread: the grants go over). Each grant adds an attempt to the operation and
/// to its row; once neither fits, the record says `complete: false` and the row
/// keeps the attempts it had.
#[test]
fn attempts_past_the_bound_are_left_out() {
    let calls: Vec<Event> = (0..10u64)
        .map(|i| queued("r", t(10), 10 + u8::try_from(i).expect("small"), i))
        .collect();
    let bound = unbounded(ROW_CAP, &calls).bytes() + 1_000;
    let mut r = bounded(ROW_CAP, bound, &calls);
    for seq in 1..=10 {
        for op in 0..10 {
            step(&mut r, granted(op, seq * 100 + op, "node-a", t(11)));
        }
    }
    let inv = r.get(&key("r")).expect("r");
    let attempts: usize = inv.rows().iter().map(|row| row.attempts.len()).sum();
    assert!(attempts > 0, "the attempts that fit are copied");
    assert!(attempts < 100, "not every attempt fits");
    assert!(!inv.complete());
}

/// Catches a record made past the bound (plant "make the record, then check": the
/// new records go over). With only open records held, a call of a new id makes no
/// record; that caller's next call, once there is room, makes it incomplete.
#[test]
fn a_record_that_does_not_fit_is_not_made_and_its_next_one_is_incomplete() {
    let calls = [queued("a", t(0), 10, 1), queued("b", t(0), 11, 2)];
    let bound = unbounded(ROW_CAP, &calls).bytes();
    let mut r = bounded(ROW_CAP, bound, &calls);
    step(&mut r, ev(call("late", t(1), CallKind::Downloaded(1))));
    assert!(r.get(&key("late")).is_none(), "no room: no record");
    // `a` finishes, goes idle and is evicted for `late`'s next call.
    step(&mut r, finished_ok(1, 1, 20, 0, t(2)));
    step(&mut r, ev(call("late", t(700), CallKind::Downloaded(1))));
    assert!(r.get(&key("a")).is_none(), "the idle record made room");
    assert!(!complete(&r, "late"), "its first call was left out");
    assert!(complete(&r, "b"));
}

/// Catches header growth past the bound (plant "insert every instance": the calls
/// go over). An instance name that does not fit is not listed, and the record says
/// `complete: false`; a call whose name is left out is not counted either (a reader
/// is asked about the names listed only).
#[test]
fn an_instance_name_past_the_bound_is_left_out() {
    let calls = [queued("a", t(0), 10, 1)];
    let bound = unbounded(ROW_CAP, &calls).bytes() + 100;
    let mut r = bounded(ROW_CAP, bound, &calls);
    for n in 0..20 {
        let mut c = call("a", t(1), CallKind::Downloaded(1));
        c.instance = format!("instance-with-a-long-name-{n:04}");
        step(&mut r, ev(c));
    }
    let inv = r.get(&key("a")).expect("a");
    assert!(inv.instances().len() > 1, "the names that fit are listed");
    assert!(inv.instances().len() < 21, "not every name fits");
    assert!(!inv.complete());
    let listed = u64::try_from(inv.instances().len() - 1).expect("a count");
    assert_eq!(
        inv.counts().bytes_downloaded,
        listed,
        "a call is counted only under a listed name"
    );
}

/// Catches the operations left out of the bound (plant `Operation::bytes` = 0: the
/// total does not grow with a call that takes no row). Past the row cap an Execute
/// still waits on an operation, which counts; one that does not fit is not waited
/// on, and the record says `complete: false`.
#[test]
fn the_operations_calls_wait_on_count_toward_the_bound() {
    let first = [queued("a", t(0), 10, 1)];
    let one = unbounded(0, &first).bytes();
    let two = unbounded(0, &[queued("a", t(0), 10, 1), queued("a", t(0), 11, 2)]).bytes();
    assert!(two > one, "a waiting call costs bytes without a row");
    let mut r = bounded(0, two + (two - one) / 2, &first);
    step(&mut r, queued("a", t(0), 11, 2));
    assert!(complete(&r, "a"));
    step(&mut r, queued("a", t(0), 12, 3));
    assert!(!complete(&r, "a"), "the third waiter does not fit");
    // The finish of the operation not waited on reaches nothing.
    step(&mut r, finished_ok(3, 1, 20, 0, t(1)));
    step(&mut r, finished_ok(1, 1, 20, 0, t(1)));
    step(&mut r, finished_ok(2, 1, 20, 0, t(1)));
    let inv = r.get(&key("a")).expect("a");
    assert_eq!(inv.counts().ran, 3, "every call is counted");
    assert_eq!(inv.state(t(1)), kbf_invocation::State::Open);
    assert_eq!(r.operations_bytes(), 0, "no operation is left");
}

/// Catches an outcome's text set past the bound (plant "keep the message": the
/// finish goes over by up to 1 KiB). The outcome stays, without its message, and the
/// record says `complete: false`; the attempt's end, with its 1 KiB reason, is not
/// copied either.
#[test]
fn a_failure_message_past_the_bound_is_left_out() {
    let calls = [queued("a", t(0), 10, 1), granted(1, 1, "node-a", t(1))];
    let bound = unbounded(ROW_CAP, &calls).bytes() + 100;
    let mut r = bounded(ROW_CAP, bound, &calls);
    step(
        &mut r,
        finished(
            1,
            Some((lease(1), LeaseEnd::Failed(Failure::Infra, "r".repeat(1024)))),
            Outcome::Failed {
                code: 13,
                message: "m".repeat(1024),
            },
            t(2),
        ),
    );
    let inv = r.get(&key("a")).expect("a");
    assert_eq!(
        inv.rows()[0].outcome,
        Some(Outcome::Failed {
            code: 13,
            message: String::new()
        })
    );
    assert_eq!(inv.rows()[0].attempts[0].end, None);
    assert!(!inv.complete());
    assert_eq!(inv.counts().failed, 1);
}

/// Catches a row replaced past the bound (plant "replace, then check": the second
/// Execute's longer name goes over). A later Execute of an action whose row does not
/// fit leaves the row as it was, and the record says `complete: false`.
#[test]
fn a_row_replaced_past_the_bound_is_left_as_it_was() {
    let calls = [queued("a", t(0), 10, 1)];
    let bound = unbounded(ROW_CAP, &calls).bytes() + 50;
    let mut r = bounded(ROW_CAP, bound, &calls);
    let again = ev(call(
        "a",
        t(1),
        CallKind::Execute {
            action: d(10),
            answer: ExecuteAnswer::Joined {
                name: format!("operations/{}", "n".repeat(200)),
                operation: OperationId(1),
            },
        },
    ));
    step(&mut r, again);
    let inv = r.get(&key("a")).expect("a");
    assert_eq!(
        inv.rows()[0].operation.as_deref(),
        Some(name("a", 1).as_str())
    );
    assert!(!inv.complete());
    assert_eq!(inv.counts().joined, 1);
}

/// Catches a waiter looked up after making room evicted its record (it would
/// panic): `m`, minted and waiting on the same operation as `r`, is the room `r`'s
/// listing makes. The finish still reaches `r`.
#[test]
fn a_waiter_evicted_while_its_operation_fills_in_is_passed_over() {
    let mut m = call(
        "m",
        t(0),
        CallKind::Execute {
            action: d(11),
            answer: ExecuteAnswer::Joined {
                name: name("m", 1),
                operation: OperationId(1),
            },
        },
    );
    m.minted = true;
    let calls = [queued("r", t(0), 11, 1), ev(m)];
    // Room for `m`'s wait, admitted as if its operation were new; not for the listing.
    let bound = unbounded(ROW_CAP, &calls).bytes() + 200;
    let mut r = bounded(ROW_CAP, bound, &calls);
    assert!(complete(&r, "m"), "m waits on the operation");
    step(&mut r, finished_with(1, 8, t(1)));
    assert!(r.get(&key("m")).is_none(), "the minted record made room");
    let inv = r.get(&key("r")).expect("r");
    assert_eq!(
        inv.rows()[0].listing.as_ref().map(|l| l.outputs.len()),
        Some(8)
    );
    assert!(inv.complete());
}

/// Catches a grant whose room evicted every waiter of its operation, looked up after
/// (it would panic): the grant reaches nothing, and the operation is gone.
#[test]
fn a_grant_whose_room_evicts_its_only_waiter_reaches_nothing() {
    let mut m = call(
        "m",
        t(0),
        CallKind::Execute {
            action: d(11),
            answer: ExecuteAnswer::Queued {
                name: name("m", 1),
                operation: OperationId(1),
            },
        },
    );
    m.minted = true;
    let calls = [ev(m), queued("r", t(0), 12, 2)];
    let bound = unbounded(ROW_CAP, &calls).bytes();
    let mut r = bounded(ROW_CAP, bound, &calls);
    step(&mut r, granted(1, 1, "node-a", t(1)));
    assert!(r.get(&key("m")).is_none(), "the minted record made room");
    assert_eq!(r.evicted(), 1);
    step(&mut r, finished_ok(1, 1, 20, 0, t(2)));
    assert!(complete(&r, "r"));
}

/// Catches a waiter looked up after a grant's room for an earlier waiter's attempt
/// evicted its record (it would panic): the operation has room for its own attempt,
/// and `r`'s copy of it evicts `m`, minted and waiting on the same operation.
#[test]
fn a_waiter_evicted_while_its_attempts_are_copied_is_passed_over() {
    let mut m = call(
        "m",
        t(0),
        CallKind::Execute {
            action: d(11),
            answer: ExecuteAnswer::Joined {
                name: name("m", 1),
                operation: OperationId(1),
            },
        },
    );
    m.minted = true;
    let calls = [queued("r", t(0), 11, 1), ev(m)];
    let before = unbounded(ROW_CAP, &calls).bytes();
    let mut granted_too = calls.to_vec();
    granted_too.push(granted(1, 1, "node-a", t(1)));
    // The grant adds three attempts: the operation's and each row's.
    let three = unbounded(ROW_CAP, &granted_too).bytes() - before;
    let mut r = bounded(ROW_CAP, before + three / 2, &calls);
    step(&mut r, granted(1, 1, "node-a", t(1)));
    assert!(r.get(&key("m")).is_none(), "the minted record made room");
    let inv = r.get(&key("r")).expect("r");
    assert_eq!(inv.rows()[0].attempts.len(), 1);
    assert!(inv.complete());
}
