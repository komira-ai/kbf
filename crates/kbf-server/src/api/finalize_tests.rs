//! The pure parts of `POST /v1/finalize` and `GET /v1/finalize`: the body, the answer of each
//! outcome and the status of each failure. The routes over a real listener, the gates and the
//! members they read are in `tests/api_finalize.rs`.

use kbf_state::StoreError;

use super::*;
use crate::members::FinalizeState;

async fn body(answer: Response) -> (StatusCode, serde_json::Value) {
    let code = answer.status();
    let bytes = axum::body::to_bytes(answer.into_body(), usize::MAX)
        .await
        .expect("a body");
    (code, serde_json::from_slice(&bytes).expect("JSON"))
}

/// The answer a call that failed never builds.
fn no_success() -> Response {
    panic!("the failure was answered as a success")
}

/// Asserts that a body names the log version to raise to, or none: no body, `{}` and
/// `{"target": null}` ask for the newest version the build runs, a target within 1 to that
/// version is taken as it is, and a target of 0 and one past the build are refused saying the
/// range, and a field that is not `target` and a body that is not JSON are refused saying what
/// a body is.
#[test]
fn a_body_names_a_target_within_what_the_build_runs() {
    for text in ["", "{}", r#"{"target":null}"#] {
        assert_eq!(target_of(text.as_bytes(), 5), Ok(5), "{text:?}");
    }
    assert_eq!(target_of(br#"{"target":1}"#, 5), Ok(1));
    assert_eq!(target_of(br#"{"target":4}"#, 5), Ok(4));
    assert_eq!(target_of(br#"{"target":5}"#, 5), Ok(5));
    let past = target_of(br#"{"target":6}"#, 5).expect_err("past what the build runs");
    assert!(past.contains("1 to 5"), "{past}");
    let zero = target_of(br#"{"target":0}"#, 5).expect_err("zero");
    assert!(zero.contains("1 to 5"), "{zero}");
    for text in [r#"{"to":3}"#, "not json", r#"{"target":-1}"#] {
        let said = target_of(text.as_bytes(), 5).expect_err(text);
        assert!(said.contains("a finalize body is"), "{text}: {said}");
    }
}

/// Asserts that a finalize that wrote the target answers 200 with the target and
/// `written: true`, one that found it at or above the target answers 200 with `written:
/// false`, and one that members block answers 409 with the target and each blocking member as
/// the report names it.
#[tokio::test]
async fn each_outcome_is_answered_with_its_status_and_its_body() {
    let (code, wrote) = body(outcome_answer(5, Outcome::Set(true))).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(wrote, serde_json::json!({"target": 5, "written": true}));
    let (code, found) = body(outcome_answer(5, Outcome::Set(false))).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(found, serde_json::json!({"target": 5, "written": false}));
    let node = FinalizeNode {
        member: 2,
        build_commit: None,
        runs_log: Some(4),
        log_version: Some(4),
        state: FinalizeState::Blocked,
        reason: "its build runs log versions up to 4, below 5".to_owned(),
    };
    let (code, blocked) = body(outcome_answer(5, Outcome::Blocked(vec![node.clone()]))).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(blocked["target"], 5);
    let listed = serde_json::to_value([node]).expect("JSON");
    assert_eq!(blocked["blocked"], listed);
    let error = blocked["error"].as_str().unwrap_or_default();
    assert!(error.contains("blocked by 1 member"), "{error}");
}

/// Asserts that a finalize call answers what its blocking part returned: the value through
/// `ok` with 200, a store that is down or contended as 503 (to be retried), an unreadable row
/// as 500 and a panicked task as 500 naming the task, each with the failure's text.
#[tokio::test]
async fn a_failure_of_the_blocking_call_is_answered_with_its_status() {
    let (code, ok) = body(answered(Ok(Ok(7)), |n| json(StatusCode::OK, &n))).await;
    assert_eq!((code, ok), (StatusCode::OK, serde_json::json!(7)));
    let store = || MembershipError::Store(StoreError::Failed("down".to_owned()));
    let failures = [
        (store(), StatusCode::SERVICE_UNAVAILABLE),
        (MembershipError::Contended, StatusCode::SERVICE_UNAVAILABLE),
        (
            MembershipError::Corrupt("finalize".to_owned()),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    ];
    for (failure, want) in failures {
        let text = failure.to_string();
        let answer = answered(Ok(Err::<u32, _>(failure)), |_| no_success());
        let (code, answer) = body(answer).await;
        assert_eq!(code, want, "{text}");
        assert_eq!(answer["error"], text);
    }
    let panicked = tokio::task::spawn_blocking(|| -> Result<u32, MembershipError> {
        panic!("the membership call panicked")
    })
    .await;
    let answer = answered::<u32>(Err(panicked.expect_err("a panic")), |_| no_success());
    let (code, answer) = body(answer).await;
    assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
    let said = answer["error"].as_str().unwrap_or_default();
    assert!(said.contains("membership task"), "{said}");
}
