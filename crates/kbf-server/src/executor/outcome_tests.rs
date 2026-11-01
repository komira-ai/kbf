use std::collections::BTreeMap;

use kbf_lease_client::{Outcome, Run, RunState};
use kbf_proto::reapi::{ActionResult, Digest};
use kbf_proto::worker::{Lost, MemoryKill};
use kbf_types::LeaseId;
use tonic::Code;

use super::{Settled, Verdict, judge, ran, refused_for_files, result_of};

fn run(state: RunState, outcome: Option<Outcome>, exit_code: Option<i32>) -> Run {
    Run {
        run_id: "run-1-1".parse().expect("a run id"),
        state,
        outcome,
        exit_code,
        detail: Some("why".to_owned()),
        labels: BTreeMap::new(),
    }
}

fn infra(verdict: Verdict) -> String {
    match verdict {
        Verdict::Settled(Settled::Infra(why)) => why,
        other => panic!("not infra: {other:?}"),
    }
}

/// Catches each ending of a run being mapped to the wrong class of the farm: one row per
/// ending, with the truncated wall that turns a timeout into the farm's loss.
#[test]
fn every_ending_has_its_class() {
    use Outcome::*;
    use RunState::{Ended, Exited, NeverStarted};
    let ended = |outcome, exit| run(Exited, Some(outcome), exit);
    assert_eq!(judge(&ended(Succeeded, Some(0)), false), Verdict::Ingest);
    assert_eq!(judge(&ended(Succeeded, None), false), Verdict::Ingest);
    assert_eq!(
        judge(&run(Ended, Some(Succeeded), Some(0)), false),
        Verdict::Ingest
    );
    assert!(infra(judge(&ended(Succeeded, Some(3)), false)).contains("nonzero"));
    assert!(infra(judge(&ended(Failed, Some(12)), false)).contains("task tool failed"));
    assert!(infra(judge(&ended(Killed, None), false)).contains("killed unasked"));
    assert!(infra(judge(&ended(FailedToStart, None), false)).contains("could not start"));
    assert!(infra(judge(&ended(Unknown, None), false)).contains("does not know"));
    assert!(infra(judge(&run(Ended, None, None), false)).contains("no outcome"));
    assert_eq!(
        judge(&ended(TimedOut, None), false),
        Verdict::Settled(Settled::Timeout)
    );
    assert_eq!(
        judge(&ended(TimedOut, None), true),
        Verdict::Settled(Settled::Lost(Lost::Fenced))
    );
    assert_eq!(
        judge(&ended(OutOfMemory, None), false),
        Verdict::Settled(Settled::OutOfMemory)
    );
    assert_eq!(
        judge(&run(NeverStarted, None, None), false),
        Verdict::Settled(Settled::Lost(Lost::DaemonRestarted))
    );
}

/// Catches the infra reason leaving out the exit code the lease server reported, which is what
/// an operator has to go on in the answer (the detail is in the log).
#[test]
fn an_infra_reason_carries_the_exit_code() {
    let why = infra(judge(
        &run(RunState::Exited, Some(Outcome::Failed), Some(13)),
        false,
    ));
    assert!(why.contains("Some(13)"), "{why}");
}

fn code_of(result: &kbf_proto::worker::Result) -> i32 {
    result.status.as_ref().expect("a status").code
}

/// Catches a class that reaches the farm as another: the status code, and the two fields
/// the farm reads in place of it (`memory_kill`, `lost`), for each way a run is settled.
#[test]
fn a_settled_run_becomes_the_result_the_farm_classifies() {
    let (lease, action) = (
        LeaseId::new(4, 9),
        Digest {
            hash: "cd".repeat(32),
            size_bytes: 5,
        },
    );
    let of = |settled| result_of(lease, &action, &settled);
    let infra = of(Settled::Infra("disk".to_owned()));
    assert_eq!(code_of(&infra), Code::Unavailable as i32);
    assert_eq!((infra.memory_kill, infra.lost), (0, 0));
    assert_eq!(infra.status.as_ref().expect("a status").message, "disk");
    assert_eq!(infra.lease_id.map(|l| (l.term, l.seq)), Some((4, 9)));
    assert_eq!(infra.action_digest.as_ref(), Some(&action));
    assert!(infra.action_result.is_none());

    let timeout = of(Settled::Timeout);
    assert_eq!(code_of(&timeout), Code::DeadlineExceeded as i32);

    let oom = of(Settled::OutOfMemory);
    assert_eq!(code_of(&oom), Code::ResourceExhausted as i32);
    assert_eq!(oom.memory_kill, MemoryKill::OwnLimit as i32);

    let lost = of(Settled::Lost(Lost::Fenced));
    assert_eq!(code_of(&lost), Code::Unavailable as i32);
    assert_eq!(lost.lost, Lost::Fenced as i32);
    assert_eq!(
        of(Settled::Lost(Lost::DaemonRestarted)).lost,
        Lost::DaemonRestarted as i32
    );

    let invalid = of(Settled::Invalid("no image".to_owned()));
    assert_eq!(code_of(&invalid), Code::InvalidArgument as i32);
}

/// Catches a result that ran with a status (it would not be OK) or without the action digest
/// the farm checks it against.
#[test]
fn a_result_that_ran_is_ok_and_names_its_action() {
    let (lease, action) = (
        LeaseId::new(4, 9),
        Digest {
            hash: "cd".repeat(32),
            size_bytes: 5,
        },
    );
    let result = ran(
        lease,
        &action,
        ActionResult {
            exit_code: 2,
            ..ActionResult::default()
        },
    );
    assert!(result.status.is_none());
    assert_eq!(result.action_result.map(|r| r.exit_code), Some(2));
    assert_eq!(result.action_digest.as_ref(), Some(&action));
}

/// Catches a host's detail text reaching the caller: it is a driver's, and a driver's error can
/// quote the arguments, which hold the pack url and the read token. The caller gets the fixed
/// phrase and the exit code; the detail goes to the log.
#[test]
fn a_hosts_detail_never_reaches_the_caller() {
    let mut leaky = run(RunState::Exited, Some(Outcome::Failed), Some(12));
    leaky.detail = Some("exec failed: --pack-url https://store/key?sig=SECRET".to_owned());
    let said = infra(judge(&leaky, false));
    assert!(
        !said.contains("SECRET") && !said.contains("exec failed"),
        "{said}"
    );
    assert!(
        said.contains("task tool failed") && said.contains("12"),
        "{said}"
    );
}

/// Catches a start the host refused for its scratch quota (`quota-unavailable`, a wire detail
/// of a run that failed to start) being the action's infrastructure failure: it is the
/// host's, so the farm runs the action again elsewhere without charging the caller's retry
/// budget, as it does for a host it lost. Only that detail on that outcome: another refusal
/// of a start (including `host-tainted`, which has its own handling) and the same words on a
/// run that did start stay infrastructure.
#[test]
fn a_start_refused_for_the_quota_is_lost_to_the_host() {
    let refused = |outcome, detail: &str| Run {
        detail: Some(detail.to_owned()),
        ..run(RunState::Exited, Some(outcome), None)
    };
    assert_eq!(
        judge(&refused(Outcome::FailedToStart, "quota-unavailable"), false),
        Verdict::Settled(Settled::Lost(Lost::Fenced))
    );
    for (outcome, detail) in [
        (Outcome::FailedToStart, "host-tainted"),
        (Outcome::FailedToStart, "scratch-failed"),
        (Outcome::FailedToStart, "quota-unavailable-ish"),
        (Outcome::FailedToStart, ""),
        (Outcome::Failed, "quota-unavailable"),
    ] {
        let verdict = judge(&refused(outcome, detail), false);
        assert!(
            matches!(verdict, Verdict::Settled(Settled::Infra(_))),
            "{outcome:?} {detail:?}: {verdict:?}"
        );
    }
    let mut bare = refused(Outcome::FailedToStart, "");
    bare.detail = None;
    assert!(matches!(
        judge(&bare, false),
        Verdict::Settled(Settled::Infra(_))
    ));
}

/// Catches the agent's own refusal of files (the detail of a run that failed to start) being a
/// plain infrastructure failure that leaves the host in use: `files-unsupported` on a start
/// that failed is recognised, and still the action's infrastructure failure to rerun, with a
/// fixed phrase; any other detail, or the same words on a run that did start, is not.
#[test]
fn a_start_the_agent_refused_for_files_is_recognised() {
    let refused = |outcome, detail: &str| Run {
        detail: Some(detail.to_owned()),
        ..run(RunState::Exited, Some(outcome), None)
    };
    let files = refused(Outcome::FailedToStart, "files-unsupported");
    assert!(refused_for_files(&files));
    let said = infra(judge(&files, false));
    assert!(said.contains("files"), "{said}");
    for (outcome, detail) in [
        (Outcome::FailedToStart, "files-unsupported-ish"),
        (Outcome::FailedToStart, "quota-unavailable"),
        (Outcome::Failed, "files-unsupported"),
    ] {
        assert!(!refused_for_files(&refused(outcome, detail)), "{detail}");
    }
}

/// Catches the agent's `invalid-file` being rerun for ever: a file the agent refuses is ours
/// to fix, so the run ends invalid, with a fixed phrase that repeats nothing of the detail.
#[test]
fn a_start_the_agent_refused_for_a_file_is_invalid() {
    let refused = Run {
        detail: Some("invalid-file".to_owned()),
        ..run(RunState::Exited, Some(Outcome::FailedToStart), None)
    };
    let Verdict::Settled(Settled::Invalid(said)) = judge(&refused, false) else {
        panic!("not invalid");
    };
    assert!(said.contains("file"), "{said}");
    let other = Run {
        outcome: Some(Outcome::Failed),
        ..refused
    };
    assert!(matches!(
        judge(&other, false),
        Verdict::Settled(Settled::Infra(_))
    ));
}
