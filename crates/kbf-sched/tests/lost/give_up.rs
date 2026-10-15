//! What the farm's budget charges and what it leaves free, at the give-ups (issue #22):
//! a lease given up for silence or replacement is charged only if its `Start` went out
//! and nothing is on its way to the log; a lease given up for reconnection
//! or a session switch is never charged to the farm budget, however often; a lease
//! given up as not started has its own budget ([`NOT_STARTED_RERUNS`]), and the two
//! never spend each other. A fault of every farm kind spends the one budget, in any
//! order.
//!
//! A module of the `lost` test binary, for its farm harness.

use std::time::Duration;

use kbf_sched::fence::{HANDOVER_GRACE, LEASE_GRACE, SESSION_SWITCH_GRACE, START_GRACE};
use kbf_sched::{Event, FARM_RERUNS, Input, NOT_STARTED_RERUNS, NodePhase, OpState, RequeueReason};
use kbf_types::{
    ControlRecord, Effect, Failure, LeaseGrant, LeaseKind, LostWhy, OperationId, Outcome,
    ResultRecord, StartLease, StateMachine, WaiterId,
};

use super::{
    FENCED, Farm, GIB, INFRA, PRESSURE, answer_in, busy, infra, lost, ok, request, running_on_a, w,
};

/// More runs than the budget allows, so a charge that crept in would end the operation.
const ROUNDS: u32 = FARM_RERUNS + 3;

/// The lease grants proposed in `effects`.
fn grants(effects: &[Effect]) -> Vec<LeaseGrant> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Commit(ControlRecord::Lease(g)) => Some(g.clone()),
            _ => None,
        })
        .collect()
}

/// Whether `effects` answer anyone or propose a result.
fn ends_the_operation(effects: &[Effect]) -> bool {
    effects.iter().any(|e| {
        matches!(
            e,
            Effect::Answer(_) | Effect::Commit(ControlRecord::Result(_))
        )
    })
}

/// A tick, fed to the scheduler without committing what it proposes.
fn raw_tick(f: &mut Farm) -> Vec<Effect> {
    f.s.apply(Input::new(f.now, Event::Tick))
}

/// Passes `by` of farm time.
fn pass(f: &mut Farm, by: Duration) {
    f.now = f.now.saturating_add(by);
}

/// Asserts that nothing has been charged to `op`, which is not done.
fn assert_unspent(f: &Farm, op: OperationId, why: &str) {
    assert_eq!(f.s.farm_runs(op), Some(&[][..]), "{why}: a run was charged");
    assert!(
        f.s.state(op).is_some_and(|s| !s.is_done()),
        "{why}: the operation ended"
    );
}

/// Catches the mutant that charges every silent lease (the guard in
/// `Scheduler::unproven` deleted): a node that goes silent before the grant is
/// committed, so the `Start` was never sent, ran nothing. The lease is requeued as
/// silent and charged nothing, more often than the budget allows, and nothing is
/// proposed or answered.
#[test]
fn silence_before_the_start_is_sent_charges_nothing() {
    let mut f = Farm::new();
    let nodes = ["a", "b"];
    for name in nodes {
        f.node(name);
    }
    f.submit(1, request(1, LeaseKind::Action));
    let mut proposed = raw_tick(&mut f);
    for round in 0..ROUNDS {
        let [grant] = &grants(&proposed)[..] else {
            panic!("round {round}: want one grant: {proposed:?}");
        };
        let silent = grant.worker.clone();
        pass(&mut f, LEASE_GRACE + Duration::from_secs(1));
        for name in nodes.iter().filter(|n| **n != silent.as_str()) {
            assert!(f.beat(name, &[]).is_empty());
        }
        // The grant was never committed: the lease is held, and no `Start` went out.
        proposed = raw_tick(&mut f);
        assert!(
            !ends_the_operation(&proposed),
            "round {round}: {proposed:?}"
        );
        assert_eq!(f.requeues(), [RequeueReason::Silent], "round {round}");
        assert_unspent(&f, grant.operation, "silence before the Start");
        // The node comes back, for the next round.
        assert!(f.beat(silent.as_str(), &[]).is_empty());
    }
}

/// Pins a known gap, not a goal (the lead decides whether to close it): a worker's
/// `Infra` result is on its way to the log when its node goes silent. The lease is
/// given up for silence, requeued and charged nothing (the result is not known to be a
/// farm fault, and the log order decides); the result then commits for the lease the
/// operation no longer holds, and finds nothing to charge it to. The run is free, so
/// `1 + FARM_RERUNS` runs ended as farm faults is a bound only without this race. If
/// the gap is closed, this test changes to charge `a` one `Infra` run.
#[test]
fn a_farm_fault_result_committed_after_the_give_up_is_not_charged() {
    let (mut f, start) = running_on_a();
    let record = ControlRecord::Result(ResultRecord {
        lease: start.lease,
        operation: start.operation,
        outcome: INFRA,
    });
    let report = Event::Report {
        operation: start.operation,
        lease: start.lease,
        outcome: INFRA,
    };
    assert_eq!(
        f.s.apply(Input::new(f.now, report)),
        [Effect::Commit(record.clone())]
    );
    pass(&mut f, LEASE_GRACE + Duration::from_secs(1));
    assert!(f.beat("b", &[]).is_empty());
    let given_up = raw_tick(&mut f);
    assert!(!ends_the_operation(&given_up), "{given_up:?}");
    assert_eq!(f.requeues(), [RequeueReason::Silent]);

    let effects = f.feed(Event::Committed(record));
    assert!(answer_in(&effects).is_none(), "{effects:?}");
    assert_eq!(f.s.farm_runs(start.operation).unwrap(), []);
    assert!(f.s.state(start.operation).is_some_and(|s| !s.is_done()));
}

/// Catches silence deciding against a worker that finished: the worker's OK result is
/// on its way to the log when its node goes silent. The lease is given up, charged
/// nothing, and no fenced result is proposed beside the worker's; the worker's result
/// commits first in the log and decides the operation, so the callers get the output.
#[test]
fn silence_after_an_ok_result_is_proposed_lets_the_result_decide() {
    let (mut f, start) = running_on_a();
    let record = ControlRecord::Result(ResultRecord {
        lease: start.lease,
        operation: start.operation,
        outcome: ok(),
    });
    let report = Event::Report {
        operation: start.operation,
        lease: start.lease,
        outcome: ok(),
    };
    assert_eq!(
        f.s.apply(Input::new(f.now, report)),
        [Effect::Commit(record.clone())]
    );
    pass(&mut f, LEASE_GRACE + Duration::from_secs(1));
    assert!(f.beat("b", &[]).is_empty());
    let given_up = raw_tick(&mut f);
    assert!(
        !ends_the_operation(&given_up),
        "a result beside it: {given_up:?}"
    );
    assert_eq!(f.requeues(), [RequeueReason::Silent]);
    assert_unspent(&f, start.operation, "silence after a result");

    let effects = f.feed(Event::Committed(record));
    let answer = answer_in(&effects).expect("the worker's result answers");
    assert_eq!(answer.outcome, ok());
    assert_eq!(answer.lease, start.lease);
    assert!(answer.farm_runs.is_empty());
}

/// The node holding `start` heartbeats without listing it, after the start grace: the
/// lease is given up as not started and the operation is placed again. The next `Start`.
fn drop_start(f: &mut Farm, start: &StartLease) -> StartLease {
    // The earlier requeues are not this give-up's.
    f.requeues();
    pass(f, START_GRACE);
    // The other nodes heartbeat, so they are alive when the operation is placed again.
    for name in ["a", "b"]
        .into_iter()
        .filter(|n| *n != start.worker.as_str())
    {
        f.beat(name, &[]);
    }
    let effects = f.beat(start.worker.as_str(), &[]);
    assert!(!ends_the_operation(&effects), "{effects:?}");
    assert_eq!(f.requeues(), [RequeueReason::NotStarted]);
    let next = f.start();
    assert_eq!(next.operation, start.operation);
    next
}

/// Catches a node that drops every `Start` holding an operation for ever (the old
/// uncharged, unbounded requeue), a give-up past the budget that answers without going
/// through the log, a wrong budget size (`1 + NOT_STARTED_RERUNS` give-ups end it, no
/// more, no fewer), and a never-started lease charged to `FARM_RERUNS`. The only node
/// drops every `Start`: the first `NOT_STARTED_RERUNS` give-ups requeue; the next
/// proposes `Infra`, and once committed the waiter is answered with it.
#[test]
fn a_node_that_drops_every_start_ends_the_operation_bounded() {
    let mut f = Farm::new();
    f.node("a");
    f.submit(1, request(1, LeaseKind::Action));
    let mut start = f.start();
    for _ in 0..NOT_STARTED_RERUNS {
        start = drop_start(&mut f, &start);
        assert_unspent(&f, start.operation, "a dropped Start");
    }
    pass(&mut f, START_GRACE);
    let beat = Event::Heartbeat {
        worker: w("a"),
        running: Vec::new(),
    };
    let proposed = f.s.apply(Input::new(f.now, beat));
    let record = ResultRecord {
        lease: start.lease,
        operation: start.operation,
        outcome: INFRA,
    };
    assert_eq!(
        proposed,
        [Effect::Commit(ControlRecord::Result(record.clone()))]
    );
    assert!(f.s.state(start.operation).is_some_and(|s| !s.is_done()));

    let effects = f.feed(Event::Committed(ControlRecord::Result(record)));
    let answer = answer_in(&effects).expect("the committed give-up answers");
    assert_eq!(answer.outcome, INFRA);
    assert_eq!(answer.waiters, [WaiterId(1)]);
    assert!(answer.farm_runs.is_empty(), "FARM_RERUNS was spent");
    assert_eq!(f.s.farm_runs(start.operation), Some(&[][..]));
    assert!(matches!(
        f.s.state(start.operation),
        Some(OpState::Failed {
            failure: Failure::Infra,
            ..
        })
    ));
}

/// Catches a not-started give-up that does not add the node to `avoid`: with a second
/// node, the operation must leave the node that drops `Start`s, not be placed on it
/// again, and finish there.
#[test]
fn a_dropped_start_moves_the_operation_to_the_other_node() {
    let (mut f, start) = running_on_a();
    let next = drop_start(&mut f, &start);
    assert_eq!(next.worker, w("b"));
    assert!(f.beat("b", &[&next]).is_empty());
    let effects = f.end(&next, ok());
    let answer = answer_in(&effects).expect("the run on b answers");
    assert_eq!(answer.outcome, ok());
    assert!(answer.farm_runs.is_empty());
}

/// Catches the two budgets being one: a spent not-started budget must leave the full
/// `FARM_RERUNS` Infra reruns, and the run past them ends the operation as an `Infra`
/// answer carrying every farm run. (A not-started give-up charged to `FARM_RERUNS`
/// would end it early.)
#[test]
fn not_started_give_ups_do_not_spend_the_farm_budget() {
    let mut f = Farm::new();
    f.node("a");
    f.submit(1, request(1, LeaseKind::Action));
    let mut start = f.start();
    for _ in 0..NOT_STARTED_RERUNS {
        start = drop_start(&mut f, &start);
    }
    for _ in 0..FARM_RERUNS {
        start = f.rerun(&start, INFRA);
    }
    let effects = f.end(&start, INFRA);
    let answer = answer_in(&effects).expect("past the farm budget it ends");
    assert_eq!(answer.outcome, INFRA);
    let runs = usize::try_from(FARM_RERUNS + 1).unwrap();
    assert_eq!(answer.farm_runs, vec![infra("a"); runs]);
}

/// Catches the two budgets being one the other way: spent `FARM_RERUNS` Infra reruns
/// must leave all `NOT_STARTED_RERUNS` not-started requeues, and only the give-up past
/// them ends the operation.
#[test]
fn farm_reruns_do_not_spend_the_not_started_budget() {
    let mut f = Farm::new();
    f.node("a");
    f.submit(1, request(1, LeaseKind::Action));
    let mut start = f.start();
    for _ in 0..FARM_RERUNS {
        start = f.rerun(&start, INFRA);
    }
    for _ in 0..NOT_STARTED_RERUNS {
        start = drop_start(&mut f, &start);
    }
    pass(&mut f, START_GRACE);
    let effects = f.beat("a", &[]);
    let answer = answer_in(&effects).expect("past the not-started budget it ends");
    assert_eq!(answer.outcome, INFRA);
    assert_eq!(
        answer.farm_runs,
        vec![infra("a"); usize::try_from(FARM_RERUNS).unwrap()]
    );
}

/// Catches a rejoin charged: a daemon process that registers again (a new session of
/// the same process) and lists nothing has its lease given up as reconnected, which
/// ran nothing. Charged nothing, however often.
#[test]
fn a_reconnection_charges_nothing() {
    let (mut f, mut start) = running_on_a();
    for round in 0..ROUNDS {
        let name = start.worker.as_str().to_owned();
        f.up(&name, &name, false, false);
        let effects = f.beat(&name, &[]);
        assert!(!ends_the_operation(&effects), "round {round}: {effects:?}");
        assert_eq!(f.requeues(), [RequeueReason::Reconnected], "round {round}");
        assert_unspent(&f, start.operation, "a reconnection");
        start = f.start();
    }
}

/// Catches a session switch charged: a whole-machine lease kept for the switch grace
/// and then given up (the daemon never came back) ran nothing the farm lost. Charged
/// nothing, however often.
#[test]
fn a_session_switch_given_up_charges_nothing() {
    let mut f = Farm::new();
    f.up("m", "m", false, true);
    f.submit(1, request(1, LeaseKind::WholeMachine));
    for round in 0..ROUNDS {
        let start = f.start();
        f.feed(Event::Phase {
            worker: w("m"),
            phase: NodePhase::Preparing(start.lease),
        });
        pass(&mut f, SESSION_SWITCH_GRACE);
        let effects = f.feed(Event::Tick);
        assert!(!ends_the_operation(&effects), "round {round}: {effects:?}");
        assert_eq!(
            f.requeues(),
            [RequeueReason::SessionSwitch],
            "round {round}"
        );
        assert_unspent(&f, start.operation, "a session switch");
        // The daemon comes back serving.
        f.up("m", "m", false, true);
        f.feed(Event::Phase {
            worker: w("m"),
            phase: NodePhase::Serving,
        });
    }
}

/// Catches one counter per path (the mutant: a restart, or a memory kill, counted
/// apart from the rest): a proven restart's give-up, a busy node's kill and an infra
/// result spend one budget, so the third ends the operation.
#[test]
fn a_restart_a_memory_kill_and_an_infra_result_share_one_budget() {
    let (mut f, _) = running_on_a();
    f.at(Duration::from_secs(1));
    f.up("a", "a2", true, false);
    assert!(f.beat("a", &[]).is_empty());
    assert_eq!(f.requeues(), [RequeueReason::Restarted]);
    let start = f.start();
    let start = f.rerun(&start, PRESSURE);
    let effects = f.end(&start, INFRA);
    let answer = answer_in(&effects).expect("answered after 3 runs of 3 kinds");
    assert_eq!(answer.outcome, INFRA);
    assert_eq!(
        answer.farm_runs,
        [lost("a", LostWhy::DaemonRestarted), busy("b"), infra("a")]
    );
}

/// Catches a budget that depends on the order of the faults (the mutant: silence
/// counted only when it is the last): silence first, then an infra result, then a
/// fenced run end the operation.
#[test]
fn silence_an_infra_result_and_a_lost_run_share_one_budget() {
    let mut f = Farm::new();
    let nodes = ["a", "b", "c"];
    for name in nodes {
        f.node(name);
    }
    f.submit(1, request(1, LeaseKind::Action));
    let start = f.start();
    let effects = f.go_silent(start.worker.as_str(), &nodes);
    let [start] = &Farm::starts(&effects)[..] else {
        panic!("no rerun after silence: {effects:?}");
    };
    let start = f.rerun(start, INFRA);
    let effects = f.end(&start, FENCED);
    let answer = answer_in(&effects).expect("answered after 3 mixed runs");
    assert_eq!(answer.outcome, FENCED);
    assert_eq!(
        answer.farm_runs,
        [
            lost("a", LostWhy::Fenced),
            infra("b"),
            lost("c", LostWhy::Fenced)
        ]
    );
}

/// Catches a replacement that does not move the operation while the budget lasts (the
/// mutant: `Replaced` requeued without recording the run, or without avoiding the
/// node): process `a2` replaces `a`'s daemon, the handover grace ends, and the lease is
/// requeued as replaced, recorded as a fenced run on `a`, and rerun on `b`.
#[test]
fn a_replacement_below_the_budget_reruns_on_another_node() {
    let (mut f, start) = running_on_a();
    f.at(Duration::from_secs(1));
    f.up("a", "a2", false, false);
    f.at(Duration::from_secs(1) + HANDOVER_GRACE);
    let effects = f.beat("a", &[]);
    assert!(answer_in(&effects).is_none(), "{effects:?}");
    assert_eq!(f.requeues(), [RequeueReason::Replaced]);
    assert_eq!(
        f.s.farm_runs(start.operation).unwrap(),
        [lost("a", LostWhy::Fenced)]
    );
    let next = f.start();
    assert_eq!(next.operation, start.operation);
    assert_eq!(next.worker, w("b"), "rerun on the node it was lost on");
}

/// Catches an action's own memory kill taken for the farm's fault (the mutant: the
/// `Infra` arm widened to `OutOfMemory`): below the largest node it is run again with
/// the booking doubled, and at the largest node it is answered, and neither is a farm
/// run.
#[test]
fn an_out_of_memory_kill_is_not_a_farm_fault() {
    let oom = Outcome::Failed(Failure::OutOfMemory);
    let (mut f, start) = running_on_a();
    let effects = f.end(&start, oom);
    assert!(answer_in(&effects).is_none(), "{effects:?}");
    let reasons = f.requeues();
    assert!(
        matches!(reasons[..], [RequeueReason::OutOfMemory { .. }]),
        "{reasons:?}"
    );
    assert_unspent(
        &f,
        start.operation,
        "an own-limit kill below the largest node",
    );

    // Booked at the largest node (16 GiB): nothing more to give.
    let mut f = Farm::new();
    f.node("a");
    let mut big = request(1, LeaseKind::Action);
    big.resources = kbf_types::Resources::new(1_000, 16 * GIB);
    f.submit(1, big);
    let start = f.start();
    let effects = f.end(&start, oom);
    let answer = answer_in(&effects).expect("answered at the largest node");
    assert_eq!(answer.outcome, oom);
    assert!(answer.farm_runs.is_empty(), "{:?}", answer.farm_runs);
    assert!(f.requeues().is_empty());
    assert!(f.s.state(start.operation).is_some_and(OpState::is_done));
}
