//! Which refusal a start that failed is read as: the job service's code and the agent's.

use js_proto::v1::{Job, JobResult, JobState, StartFailureCode};

use super::{Settled, Verdict, judge};
use crate::executor::refused::Refusal;

fn failed_to_start(code: StartFailureCode, detail: &str) -> Job {
    Job {
        id: "k0123abcd-4-9".to_owned(),
        state: JobState::FailedToStart.into(),
        start_failure: code.into(),
        start_failure_detail: detail.to_owned(),
        ..Job::default()
    }
}

/// Asserts each bare code an agent refuses a start with for now, as a job service that does
/// not place it again passes it on, is read as its own refusal: `no-capacity`, `host-held` and
/// `host-tainted`; and that a detail that names no room but also a refused tree list is read as
/// the tree list, which the frontend acts on.
#[test]
fn each_code_of_a_start_refused_for_now_is_its_own_refusal() {
    let agent = |detail: &str| {
        judge(
            &failed_to_start(StartFailureCode::AgentRefused, detail),
            &JobResult::default(),
        )
    };
    for (detail, refusal) in [
        ("no-capacity", Refusal::NoCapacity),
        ("host-held", Refusal::HostHeld),
        ("host-tainted", Refusal::HostTainted),
    ] {
        assert_eq!(agent(detail), Verdict::Refused(refusal), "{detail}");
    }
    for other in [
        "no-capacity-ish",
        "host-held2",
        "Host-tainted",
        " host-held",
    ] {
        assert!(
            matches!(agent(other), Verdict::Settled(Settled::Infra(_))),
            "{other}"
        );
    }
    assert_eq!(
        agent("no-capacity tree list not supported: missing_sidecar"),
        Verdict::ListRefused
    );
}

/// Asserts the one start refusal read as no host ever running the job: `platform-mismatch`,
/// answered as the caller's, and nothing else of the detail read.
#[test]
fn a_platform_mismatch_is_invalid_and_its_lookalikes_are_not() {
    let agent = |detail: &str| {
        judge(
            &failed_to_start(StartFailureCode::AgentRefused, detail),
            &JobResult::default(),
        )
    };
    let Verdict::Settled(settled @ Settled::Invalid(_)) = agent("platform-mismatch: os") else {
        panic!("not invalid");
    };
    assert!(!settled.retried());
    assert!(matches!(
        agent("platform-mismatch-ish"),
        Verdict::Settled(Settled::Infra(_))
    ));
}

/// Asserts that a start the job service placed again itself and then gave up on is not placed
/// again for free, which would retry it twice over: its own `NO_CAPACITY` (the queue wait ran
/// out), and the agent's `no-capacity` or `host-held` followed by the hosts that refused it, are
/// ordinary `infra` endings, run again as the operation's next attempt, and the hosts' names
/// never reach the answer.
#[test]
fn a_start_the_job_service_already_retried_is_an_ordinary_infra_ending() {
    let judged = |code: StartFailureCode, detail: &str| {
        judge(&failed_to_start(code, detail), &JobResult::default())
    };
    let infra = |verdict: Verdict| match verdict {
        Verdict::Settled(settled @ Settled::Infra(_)) => {
            assert!(settled.retried());
            settled.status().message
        }
        other => panic!("not infra: {other:?}"),
    };
    assert_eq!(
        infra(judged(StartFailureCode::NoCapacity, "")),
        "the job failed to start: no-capacity"
    );
    for (detail, code) in [
        ("no-capacity: host-a, host-b", "no-capacity"),
        ("host-held: host-a", "host-held"),
    ] {
        assert_eq!(
            infra(judged(StartFailureCode::AgentRefused, detail)),
            format!(
                "the host refused to start the job ({code}), and the job service placed it \
                 again until it gave up"
            ),
            "{detail}"
        );
    }
}
