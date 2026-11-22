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

/// Asserts each code a host or the job service refuses a start with for now is read as its own
/// refusal: the job service's `NO_CAPACITY`, and the agent's `no-capacity`, `host-held` and
/// `host-tainted`, also with text after the code; and that a refusal that names no room but
/// also a refused tree list is read as the tree list, which the frontend acts on.
#[test]
fn each_code_of_a_start_refused_for_now_is_its_own_refusal() {
    let agent = |detail: &str| {
        judge(
            &failed_to_start(StartFailureCode::AgentRefused, detail),
            &JobResult::default(),
        )
    };
    assert_eq!(
        judge(
            &failed_to_start(StartFailureCode::NoCapacity, ""),
            &JobResult::default()
        ),
        Verdict::Refused(Refusal::NoCapacity)
    );
    for (detail, refusal) in [
        ("no-capacity", Refusal::NoCapacity),
        (
            "no-capacity: the job does not fit what is free",
            Refusal::NoCapacity,
        ),
        ("host-held", Refusal::HostHeld),
        ("host-held: draining for an update", Refusal::HostHeld),
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
