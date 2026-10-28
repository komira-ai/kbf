use kbf_front::Cache;
use std::sync::Arc;

use super::*;

/// Catches a server process that reuses an earlier process's term (issue #137:
/// every process granted leases from `(1, 0)`), and one whose term does not order
/// after that of a process started a few milliseconds before. A term is also never
/// 1, the term every server before the fix used.
#[test]
fn each_process_term_is_new_and_later() {
    let first = process_term();
    std::thread::sleep(Duration::from_millis(3));
    let second = process_term();
    assert!(second > first, "{second} does not order after {first}");
    assert!(first >> 16 > 0, "{first} could be a pre-fix term");
}

/// Catches a result the farm settles with no detail that is not a farm fault (a
/// committed `Completed` whose worker report this process never saw) being answered
/// as anything but the fixed INTERNAL "could not run the action", or panicking.
#[test]
fn a_result_without_detail_that_is_no_fault_is_answered_internal() {
    let farm = Farm::new(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    );
    let answer = kbf_types::Answer {
        operation: kbf_types::OperationId(1),
        lease: kbf_types::LeaseId::new(1, 0),
        waiters: Vec::new(),
        outcome: Outcome::Completed {
            action_result: kbf_types::Digest::new(kbf_types::DigestFunction::Sha256, [1; 32], 1),
        },
        memory_runs: Vec::new(),
        farm_runs: Vec::new(),
    };
    let settled = farm.lock().settle(&answer);
    assert_eq!(
        settled.finished,
        failed(Code::Internal, "the farm could not run the action")
    );
}

/// Catches a farm fault (Infra, node memory pressure, a lost run) with no detail being
/// answered as the plain INTERNAL, which drops the farm's runs and the operator fix.
#[test]
fn a_result_without_detail_that_is_a_farm_fault_names_the_fault() {
    for failure in [Failure::Infra, Failure::NodeMemoryPressure] {
        let finished = unrun(Outcome::Failed(failure), &[]);
        assert_eq!(finished, fault::finished(&[]), "{failure:?}");
        assert_ne!(
            finished,
            failed(Code::Internal, "the farm could not run the action")
        );
    }
}

/// Catches: a `Result` answered with an ack the farm owes for another reason, or none:
/// an accepted one says accepted, a refused one says not, and one whose record is held
/// for the state store or was dropped is not acknowledged at all.
#[test]
fn only_a_taken_or_a_refused_result_is_acknowledged() {
    assert_eq!(Taken::Accepted.ack(), Some((true, Reason::Accepted)));
    assert_eq!(
        Taken::Refused(Reason::Stale).ack(),
        Some((false, Reason::Stale))
    );
    assert_eq!(Taken::Held.ack(), None);
    assert_eq!(Taken::Dropped.ack(), None);
}

/// Catches: a report the scheduler does not take (its lease is not the operation's
/// current one: given up and granted again while the outputs were checked) classed as
/// held for the store or dropped, which would withhold the ack of a lease that is plainly
/// refused: it is refused, and nothing is answered.
#[test]
fn a_report_the_scheduler_does_not_take_is_refused() {
    let farm = Farm::new(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    );
    let worker = WorkerId::new("node-a");
    let caps = kbf_caps::NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    let capacity = kbf_types::Resources::new(4_000, 8 << 30);
    let (outbound, _inbox) = mpsc::unbounded_channel();
    farm.register(
        &worker,
        DaemonInstance::new("daemon"),
        DaemonInstance::new(""),
        Reported::serving(capacity, caps),
        outbound,
        ServerMessage::default(),
    );
    let request = kbf_sched::Request {
        key: kbf_types::ActionKey {
            instance: "main".to_owned(),
            action: Digest::new(kbf_types::DigestFunction::Sha256, [1; 32], 1),
        }
        .into(),
        qos: kbf_types::Qos::Ci,
        kind: kbf_types::LeaseKind::Action,
        resources: kbf_types::Resources::new(1_000, 1 << 30),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
    };
    farm.submit(Submission {
        request,
        call: None,
        described: kbf_front::Described::default(),
        evidence: Default::default(),
    })
    .expect("submitted");
    let report = Report {
        lease: LeaseId::new(farm.lease_epoch(), 99),
        outcome: Outcome::Failed(Failure::Infra),
        measured: None,
        exit_code: None,
        run: None,
    };
    let now = farm.now();
    let (answers, taken) = farm
        .lock()
        .take_report(now, &worker, OperationId(0), report);
    assert!(answers.is_empty());
    // The lease was never granted by this process.
    assert_eq!(taken, Taken::Refused(Reason::UnknownLease));
}

/// Catches: a request whose key is not an action's being queued, which would give its
/// callers a made-up action to be told, or panicking on it. It is refused as an invalid
/// argument, and nothing is queued.
#[test]
fn a_request_keyed_by_something_other_than_an_action_is_refused() {
    let farm = Farm::new(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    );
    let request = kbf_sched::Request {
        key: kbf_types::WorkKey::new("main", "not a digest"),
        qos: kbf_types::Qos::Ci,
        kind: kbf_types::LeaseKind::Action,
        resources: kbf_types::Resources::new(1_000, 1 << 30),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
    };
    let refused = farm.submit(Submission {
        request,
        call: None,
        described: kbf_front::Described::default(),
        evidence: Default::default(),
    });
    let Err(status) = refused else {
        panic!("a request keyed by something other than an action was queued");
    };
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("not an action digest"),
        "{status:?}"
    );
    assert_eq!(farm.lock().sched.queued_len(), 0);
}
