use std::sync::Arc;

use kbf_front::Cache;
use kbf_meta::{ActionRecord, Closure};
use kbf_proto::reapi::ActionResult;
use kbf_types::{Answer, Digest, DigestFunction, LeaseId, OperationId, Refusal};

use super::*;
use crate::farm::report::Pending;

fn farm() -> Farm {
    Farm::new_legacy(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
}

fn digest() -> Digest {
    Digest::new(DigestFunction::Sha256, [1; 32], 1)
}

fn ran(exit_code: i32) -> Detail {
    Detail::Ran(Box::new(Pending {
        result: ActionResult {
            exit_code,
            ..ActionResult::default()
        },
        record: ActionRecord {
            result: digest(),
            closure: Closure::new(),
        },
    }))
}

fn answer(outcome: Outcome) -> Answer {
    Answer {
        operation: OperationId(1),
        lease: LeaseId::new(1, 0),
        waiters: Vec::new(),
        outcome,
        memory_runs: Vec::new(),
        farm_runs: Vec::new(),
    }
}

/// Asserts that a scrape of a farm that finished nothing has the family with a header and
/// a series of each of the four classes at 0, so a gate reading it never sees an absent
/// series for a farm that has not failed.
#[test]
fn every_class_is_written_at_zero_from_the_start() {
    let mut out = String::new();
    FailedActions::default().render(&mut out);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 6, "{out}");
    assert!(
        lines[0].starts_with("# HELP kbf_execute_failed_actions_total "),
        "{out}"
    );
    assert_eq!(lines[1], "# TYPE kbf_execute_failed_actions_total counter");
    for (line, class) in lines[2..]
        .iter()
        .zip(["farm", "request", "action", "ambiguous"])
    {
        let want = format!("kbf_execute_failed_actions_total{{class=\"{class}\"}} 0");
        assert_eq!(*line, want, "{out}");
    }
}

/// Asserts that each class counts into its own series: a count that landed in another
/// class's series would show here, since the three counts differ.
#[test]
fn each_class_counts_into_its_own_series() {
    let mut failed = FailedActions::default();
    failed.count(Class::Request);
    for _ in 0..2 {
        failed.count(Class::Action);
    }
    for _ in 0..3 {
        failed.count(Class::Farm);
    }
    let mut out = String::new();
    failed.render(&mut out);
    for (class, want) in [("farm", 3), ("request", 1), ("action", 2), ("ambiguous", 0)] {
        let sample = format!("kbf_execute_failed_actions_total{{class=\"{class}\"}} {want}\n");
        assert!(out.contains(&sample), "{sample}{out}");
    }
}

/// Asserts the class of each way an operation ends: an exit 0 is no failure, a non-zero
/// exit, a timeout and a kill at the largest node's memory are the action's, an invalid
/// action is the request's, and a farm fault, a lost input, an infra failure and a result
/// the process never saw are the farm's.
///
/// Catches: labelling every failure `action`, or dropping the `exit_code` test, which would
/// fail one of these.
#[test]
fn an_operation_is_classed_by_what_its_callers_are_told() {
    let completed = Outcome::Completed {
        action_result: digest().into(),
    };
    assert_eq!(class_of(completed, Some(&ran(0))), None);
    assert_eq!(class_of(completed, Some(&ran(2))), Some(Class::Action));
    let timeout = Outcome::Failed(Failure::Timeout);
    assert_eq!(class_of(timeout, None), Some(Class::Action));
    let memory = Outcome::Failed(Failure::OutOfMemory);
    assert_eq!(class_of(memory, None), Some(Class::Action));
    let invalid = Outcome::Failed(Failure::Invalid);
    let why = Detail::Invalid("image named by tag".to_owned());
    assert_eq!(class_of(invalid, Some(&why)), Some(Class::Request));
    let lost = Detail::Missing(vec![digest()]);
    let missing = Outcome::Failed(Failure::MissingInput);
    assert_eq!(class_of(missing, Some(&lost)), Some(Class::Farm));
    for failure in [Failure::Infra, Failure::NodeMemoryPressure] {
        assert_eq!(class_of(Outcome::Failed(failure), None), Some(Class::Farm));
    }
    assert_eq!(class_of(completed, None), Some(Class::Farm));
}

/// Asserts that settling counts one operation once, in the class its callers are told,
/// whatever the number of its callers: an operation with a farm fault raises `farm` by
/// exactly 1 and the rest stay 0, and a refused operation (no worker could run it) is the
/// farm's too.
#[test]
fn settling_and_refusing_count_the_operation_once() {
    let farm = farm();
    assert_eq!(farm.failed_actions(), FailedActions::default());
    farm.lock().settle(&answer(Outcome::Failed(Failure::Infra)));
    let after = farm.failed_actions();
    assert_eq!(after.of(Class::Farm), 1);
    for class in [Class::Request, Class::Action, Class::Ambiguous] {
        assert_eq!(after.of(class), 0, "{class:?}");
    }
    farm.lock().refuse(&Refusal {
        operation: OperationId(2),
        waiters: Vec::new(),
        reason: "no worker".to_owned(),
    });
    assert_eq!(farm.failed_actions().of(Class::Farm), 2);
}

/// Asserts that a result with a non-zero exit settles into `action` and one with exit 0
/// into no class, through the detail the report parked for the lease.
#[test]
fn a_parked_result_is_counted_by_its_exit_code() {
    let farm = farm();
    for (n, exit) in [(1, 0), (2, 3)] {
        let mut lease_answer = answer(Outcome::Completed {
            action_result: digest().into(),
        });
        lease_answer.lease = LeaseId::new(1, n);
        lease_answer.operation = OperationId(n);
        let mut state = farm.lock();
        state.park(lease_answer.lease, ran(exit));
        state.settle(&lease_answer);
    }
    let failed = farm.failed_actions();
    assert_eq!(failed.of(Class::Action), 1);
    assert_eq!(failed.of(Class::Farm), 0);
}
