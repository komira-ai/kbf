//! A start a host refused because it cannot mount the job's tree list through tierstore.

use js_proto::v1::{Job, JobResult, JobState, StartFailureCode};
use tonic::Code;

use super::{
    LIST_ABSENT, LIST_NOT_SUPPORTED, LIST_REFUSALS, RESUBMITS, Settled, Verdict, judge,
    list_refused,
};
use crate::executor::{LostObject, MissPath};

fn failed_to_start(code: StartFailureCode, detail: &str) -> Job {
    Job {
        id: "k0123abcd-4-9".to_owned(),
        state: JobState::FailedToStart.into(),
        start_failure: code.into(),
        start_failure_detail: detail.to_owned(),
        ..Job::default()
    }
}

/// The text tierstore's reader gives the refusal of a tree list with no readable sidecar, built
/// by the reader itself: the executor's detection is held to it.
#[test]
fn the_text_of_tierstores_own_refusal_is_detected() {
    let refusal = tierstore_list::TreeListL2::<&[u8]>::open_sidecar(None, &[0_u8; 32])
        .expect_err("a missing sidecar is refused")
        .to_string();
    assert_eq!(refusal, "tree list not supported: missing_sidecar");
    assert!(list_refused(&refusal));
    assert!(refusal.starts_with(LIST_NOT_SUPPORTED));
}

/// Each reason tierstore gives for a tree list it does not read is detected, whatever the start
/// failure's code is.
#[test]
fn a_refusal_for_each_reason_label_is_a_refused_list_whatever_the_code() {
    for reason in [
        "v1_list",
        "missing_sidecar",
        "unknown_cell_digest",
        "unknown_version",
    ] {
        let detail = format!("tree list not supported: {reason}");
        assert!(list_refused(&detail), "{reason}");
        for code in [
            StartFailureCode::Unspecified,
            StartFailureCode::NoCapacity,
            StartFailureCode::InputUnavailable,
            StartFailureCode::AgentRefused,
        ] {
            let verdict = judge(&failed_to_start(code, &detail), &JobResult::default());
            assert_eq!(verdict, Verdict::ListRefused, "{reason} {code:?}");
        }
    }
}

/// The text is found anywhere in the detail, and a near miss is not found.
#[test]
fn the_prefix_is_found_in_the_middle_of_a_longer_detail_and_a_near_miss_is_not() {
    assert!(list_refused(
        "mount provider: open cas/l/ab: tree list not supported: v1_list (retry elsewhere)"
    ));
    for near in [
        "",
        "tree list not supported",
        "tree list not supported:v1_list",
        "Tree list not supported: v1_list",
        "tree lists not supported: v1_list",
        "tree  list not supported: v1_list",
        "list not supported: v1_list",
    ] {
        assert!(!list_refused(near), "{near:?}");
        let verdict = judge(
            &failed_to_start(StartFailureCode::InputUnavailable, near),
            &JobResult::default(),
        );
        assert!(
            matches!(verdict, Verdict::Settled(Settled::Infra(_))),
            "{near:?}"
        );
    }
}

/// A detail that holds both the refusal and a lost object is judged a refused list: the host
/// could not use the list, whatever else the store lacks, and the next attempt's refusal finds
/// the lost object if it is still lost.
#[test]
fn a_detail_with_a_refusal_and_a_lost_object_is_a_refused_list() {
    let sha = "9e1bf90c91fe575d7dacd35d1b21d058c2f1fd7abd0d23ffa41c4a0da2facb7f";
    let detail =
        format!("cannot fetch manifest {sha}: upstream 404; tree list not supported: v1_list");
    assert!(
        super::lost_of(&detail).is_some(),
        "the detail names a lost object too"
    );
    let verdict = judge(
        &failed_to_start(StartFailureCode::InputUnavailable, &detail),
        &JobResult::default(),
    );
    assert_eq!(verdict, Verdict::ListRefused);
    let lost = format!("cannot fetch manifest {sha}: upstream 404");
    let verdict = judge(
        &failed_to_start(StartFailureCode::InputUnavailable, &lost),
        &JobResult::default(),
    );
    assert!(matches!(
        verdict,
        Verdict::StoreLost {
            object: LostObject::Manifest(_),
            ..
        }
    ));
}

/// A refused list is answered UNAVAILABLE naming the step the frontend took, is run again, and
/// each operation has two runs of it that cost no attempt.
#[test]
fn a_refused_list_is_unavailable_and_run_again() {
    assert_eq!(LIST_REFUSALS, 2);
    assert_eq!(RESUBMITS, 2);
    let mut messages = Vec::new();
    for path in [MissPath::Republish, MissPath::Manifests] {
        let settled = Settled::ListRefused(path);
        assert!(settled.retried(), "{path:?}");
        let status = settled.status();
        assert_eq!(status.code, Code::Unavailable as i32, "{path:?}");
        messages.push(status.message);
    }
    assert_eq!(
        messages,
        [
            "the host refused the job's tree list as not mountable (the list was written again \
             with its sidecar)",
            "the host refused the job's tree list as not mountable (the tree is given as \
             manifests)",
        ]
    );
}

/// Catches a start whose tree list the collector deleted taken for an ordinary failure (an
/// attempt spent, and the next attempt given the same missing list from the memo): tierstore's
/// `no tree list at <key>`, anywhere in the detail and whatever the code, is a refused list; a
/// near miss is not.
#[test]
fn a_list_the_store_no_longer_has_is_a_refused_list_whatever_the_code() {
    let key = format!("cas/l/{}", "ab".repeat(32));
    let detail = format!("prepare refused: no tree list at {key}");
    assert!(detail.contains(LIST_ABSENT));
    assert!(list_refused(&detail));
    for code in [
        StartFailureCode::Unspecified,
        StartFailureCode::InputUnavailable,
        StartFailureCode::AgentRefused,
    ] {
        let verdict = judge(&failed_to_start(code, &detail), &JobResult::default());
        assert_eq!(verdict, Verdict::ListRefused, "{code:?}");
    }
    for near in [
        "no tree list",
        "no tree list at",
        "No tree list at cas/l/ab",
        "no tree lists at x",
    ] {
        assert!(!list_refused(near), "{near:?}");
    }
}
