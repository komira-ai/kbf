//! A job whose host could not provide or read its inputs: how its result is judged, and what the
//! callers are answered when it is the last attempt.

use js_proto::v1::{Job, JobResult, JobState, ResultReason, StartFailureCode};
use tonic::Code;

use super::{FailedInput, InputFailure, Settled, Verdict, judge};

/// A job in `state` on host `host-3`, with `detail` as its start failure's.
fn job(state: JobState, detail: &str) -> Job {
    Job {
        id: "k0123abcd-4-9".to_owned(),
        state: state.into(),
        start_failure: StartFailureCode::InputUnavailable.into(),
        start_failure_detail: detail.to_owned(),
        host_id: "host-3".to_owned(),
        ..Job::default()
    }
}

/// A result in `state` for `reason`, with exit code `exit_code` and the agent's `code`.
fn result(state: JobState, reason: ResultReason, exit_code: i32, code: Option<&str>) -> JobResult {
    JobResult {
        state: state.into(),
        reason: reason.into(),
        exit_code: Some(exit_code),
        input_failure: code.map(str::to_owned),
        ..JobResult::default()
    }
}

/// The verdict of an input failure on `host-3`, reported with `code`.
fn reported(code: &str) -> Verdict {
    Verdict::Settled(Settled::InputFailed(InputFailure {
        host: "host-3".to_owned(),
        input: FailedInput::Reported(code.to_owned()),
    }))
}

/// Catches an input failure read as the job's own ending: a result that names one is the host's
/// whatever the job's state, reason and exit code, a clean exit 0 included (whose pack would
/// otherwise be ingested); the reason input-failed with no code is one too.
#[test]
fn an_input_failure_is_the_hosts_whatever_the_jobs_state_and_exit() {
    use JobState::{Cancelled, Failed, Lost, Succeeded, Unspecified};
    use ResultReason::{Exit, InputFailed, Memory, WallLimit};
    let code = Some("input-unreadable");
    for (state, reason, exit_code) in [
        (Succeeded, Exit, 0),
        (Failed, Exit, 1),
        (Failed, InputFailed, 0),
        (Failed, Memory, 137),
        (Failed, WallLimit, 137),
        (Cancelled, Exit, 0),
        (Lost, Exit, 0),
        (Unspecified, Exit, 0),
    ] {
        let judged = judge(&job(state, ""), &result(state, reason, exit_code, code));
        assert_eq!(
            judged,
            reported("input-unreadable"),
            "{state:?} {reason:?} {exit_code}"
        );
    }
    let judged = judge(
        &job(Failed, ""),
        &result(Failed, ResultReason::InputFailed, 1, None),
    );
    assert_eq!(judged, reported(""));
}

/// Catches the agent's text reaching the callers beyond its code: only the leading run of
/// lower-case letters, digits and dashes is kept.
#[test]
fn only_the_agents_leading_code_is_kept() {
    let judged = judge(
        &job(JobState::Failed, ""),
        &result(
            JobState::Failed,
            ResultReason::InputFailed,
            1,
            Some("input-unreadable: /mnt/ts/trees/abc answered EIO"),
        ),
    );
    assert_eq!(judged, reported("input-unreadable"));
}

/// Catches a start that failed for a refused tree list or a lost object losing the frontend's
/// part to an input failure the result also names: those are read first, and an input failure
/// is read before the start failure's code.
#[test]
fn a_start_failure_is_read_for_the_tree_list_and_the_lost_object_before_the_input_failure() {
    let named = result(
        JobState::FailedToStart,
        ResultReason::Unspecified,
        0,
        Some("tree-read-failed"),
    );
    let list = job(
        JobState::FailedToStart,
        "tree list not supported: missing_sidecar",
    );
    assert_eq!(judge(&list, &named), Verdict::ListRefused);
    let lost = job(
        JobState::FailedToStart,
        "cannot fetch manifest 9e1bf90c91fe575d7dacd35d1b21d058c2f1fd7abd0d23ffa41c4a0da2facb7f: upstream 404",
    );
    assert!(
        matches!(judge(&lost, &named), Verdict::StoreLost { .. }),
        "{:?}",
        judge(&lost, &named)
    );
    let plain = job(JobState::FailedToStart, "");
    assert_eq!(judge(&plain, &named), reported("tree-read-failed"));
    let unnamed = JobResult::default();
    assert!(
        matches!(judge(&plain, &unnamed), Verdict::Settled(Settled::Infra(_))),
        "{:?}",
        judge(&plain, &unnamed)
    );
}

/// Catches an input failure answered as anything but the farm's, or with a message that does not
/// name the input and the host: each form is run again, and at the last attempt answered
/// `UNAVAILABLE` with exactly this message and no detail.
#[test]
fn an_input_failure_is_answered_unavailable_naming_the_input_and_the_host() {
    let cases = [
        (
            "host-3",
            FailedInput::Reported("input-unreadable".to_owned()),
            "the farm could not provide the job's inputs on host host-3: the job service \
             reported input-unreadable (not the action's own failure)",
        ),
        (
            "host-3",
            FailedInput::Reported(String::new()),
            "the farm could not provide the job's inputs on host host-3: the job service \
             reported an input failure (not the action's own failure)",
        ),
        (
            "host-3",
            FailedInput::Unread("buck-out/v2/zig/lib/libcxx/include/__assert".to_owned()),
            "the farm could not read the action's input \
             \"buck-out/v2/zig/lib/libcxx/include/__assert\" on host host-3: opening it answered \
             an I/O error (not the action's own failure)",
        ),
        (
            "",
            FailedInput::Unread("a\nb.h".to_owned()),
            "the farm could not read the action's input \"a\\nb.h\" on its host: opening it \
             answered an I/O error (not the action's own failure)",
        ),
    ];
    for (host, input, message) in cases {
        let settled = Settled::InputFailed(InputFailure {
            host: host.to_owned(),
            input,
        });
        assert!(settled.retried(), "{settled:?}");
        let status = settled.status();
        assert_eq!(status.code, Code::Unavailable as i32);
        assert_eq!(status.message, message);
        assert!(status.details.is_empty());
    }
}
