//! Which refusal a start that failed is read as: the job service's code and the agent's.

use js_proto::v1::{Job, JobResult, JobState, StartFailureCode};

use super::{CODE_BYTES, Settled, Verdict, judge};
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
/// `host-tainted`; that the job service's own `NO_CAPACITY` (no host had room within the
/// pool's queue wait), with or without a detail, is read as no room too; and that a detail that
/// names no room but also a refused tree list is read as the tree list, which the frontend acts
/// on.
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
    for detail in ["", "no host had room within 600s"] {
        assert_eq!(
            judge(
                &failed_to_start(StartFailureCode::NoCapacity, detail),
                &JobResult::default()
            ),
            Verdict::Refused(Refusal::NoCapacity),
            "{detail}"
        );
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

/// Asserts that a start the job service placed again itself on other hosts and then gave up on
/// is not placed again for free, which would retry it twice over: the agent's `no-capacity` or
/// `host-held` followed by the hosts that refused it are ordinary `infra` endings, run again as
/// the operation's next attempt, and the hosts' names never reach the answer.
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

/// Asserts that a start that failed on a code this server does not name is answered with that
/// code leading its message, so two such failures can be told apart: the agent's leading code
/// (and nothing after it), cut to [`CODE_BYTES`]; the job service's number of a start-failure
/// code it does not know; and the fixed phrase alone when there is no code to read (a detail
/// that does not start with one, or no code at all). Each is an `infra` ending, run again.
#[test]
fn a_code_this_server_does_not_name_leads_the_message() {
    let infra = |job: Job| match judge(&job, &JobResult::default()) {
        Verdict::Settled(settled @ Settled::Infra(_)) => {
            assert!(settled.retried());
            settled.status().message
        }
        other => panic!("not infra: {other:?}"),
    };
    let agent = |detail: &str| infra(failed_to_start(StartFailureCode::AgentRefused, detail));
    assert_eq!(
        agent("brand-new-code"),
        "brand-new-code: the host refused to start the job"
    );
    assert_eq!(
        agent("cgroup-v3-missing: /sys/fs/cgroup/secret token=abc"),
        "cgroup-v3-missing: the host refused to start the job"
    );
    let long = "a".repeat(CODE_BYTES + 10);
    assert_eq!(
        agent(&long),
        format!(
            "{}: the host refused to start the job",
            "a".repeat(CODE_BYTES)
        )
    );
    let exact = "b".repeat(CODE_BYTES);
    assert_eq!(
        agent(&exact),
        format!("{exact}: the host refused to start the job")
    );
    for none in ["", "Upper-case code", " leading-space", "/a/path"] {
        assert_eq!(agent(none), "the host refused to start the job", "{none:?}");
    }
    let service = |number: i32, detail: &str| {
        infra(Job {
            start_failure: number,
            ..failed_to_start(StartFailureCode::Unspecified, detail)
        })
    };
    assert_eq!(
        service(4242, "some-code: text"),
        "start-failure-4242: the job failed to start"
    );
    assert_eq!(service(0, "some-code: text"), "the job failed to start");
}
