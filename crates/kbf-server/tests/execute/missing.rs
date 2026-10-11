//! An input the worker could not fetch, end to end in one process: a REAPI client, the
//! server, and a fake daemon that reports the lease FAILED_PRECONDITION with `MISSING`
//! violations, as the real daemon does for a driver's `MissingBlob`
//! (`kbf-daemon`, `lease::missing`). The client must get the answer REAPI asks for, on
//! which Bazel re-uploads the blobs and executes again (failure classes, section 3).
//!
//! A module of the `execute` test binary, not a binary of its own: `Farm` is generic,
//! and coverage counts each generic function by its best-covered instantiation, so the
//! missing-input path of `Farm::report` must run in the same binary as its other paths.

use crate::support::{Blob, Cell, Job, done, failed, output, response};
use kbf_proto::google::rpc::precondition_failure::Violation;
use kbf_proto::google::rpc::{self, ErrorInfo, PreconditionFailure};
use prost::Message;
use tonic::Code;

const PRECONDITION_FAILURE: &str = "type.googleapis.com/google.rpc.PreconditionFailure";

fn missing(blob: &Blob) -> Violation {
    Violation {
        r#type: "MISSING".to_owned(),
        subject: format!(
            "blobs/{}/{}",
            blob.digest.hash_hex(),
            blob.digest.size_bytes
        ),
        description: String::new(),
    }
}

fn any(type_url: &str, value: Vec<u8>) -> prost_types::Any {
    prost_types::Any {
        type_url: type_url.to_owned(),
        value,
    }
}

/// Catches, for a lease the daemon reports with two `MISSING` inputs:
/// - the fixed INTERNAL "the farm could not run the action" in place of
///   FAILED_PRECONDITION (the defect this pins: Bazel retries INTERNAL as a fresh
///   Execute that fails the same way, and never re-uploads);
/// - FAILED_PRECONDITION with the `PreconditionFailure` dropped, or with a blob left
///   out of it, or naming them in another form than `blobs/<hash>/<size>`;
/// - the daemon's status forwarded as sent: its extra `ErrorInfo` reaches the client,
///   and any detail besides the `PreconditionFailure` stops Bazel re-uploading;
/// - the `ActionResult` a report carries next to the failure taken as the answer, or
///   written to the action cache (a later build would get a result that never ran);
/// - the operation run again (its farm reruns spent on the client's missing blob) or
///   the report refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_input_at_the_worker_reaches_the_client_as_missing() {
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-missing", 4, 8).await;
    let job = Job::new("its input went missing", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let start = daemon.start().await;

    let stored = output(&cell, "stored, but the lease failed", 0).await;
    let violations = vec![missing(&job.input), missing(&job.root)];
    let extra = ErrorInfo {
        reason: "FROM_THE_DAEMON".to_owned(),
        domain: "kbf".to_owned(),
        ..ErrorInfo::default()
    };
    let mut report = failed(start.lease_id, Code::FailedPrecondition);
    report.status = Some(rpc::Status {
        code: Code::FailedPrecondition as i32,
        message: "blob is not in the CAS".to_owned(),
        details: vec![
            any(
                PRECONDITION_FAILURE,
                PreconditionFailure {
                    violations: violations.clone(),
                }
                .encode_to_vec(),
            ),
            any(
                "type.googleapis.com/google.rpc.ErrorInfo",
                extra.encode_to_vec(),
            ),
        ],
    });
    report.action_result = Some(stored);
    assert!(daemon.report(report).await.accepted);

    let answer = response(&done(&mut ops).await);
    assert_eq!(answer.result, None);
    let status = answer.status.expect("a status");
    assert_eq!(
        status.code,
        Code::FailedPrecondition as i32,
        "{}",
        status.message
    );
    assert_eq!(status.message, "2 input blob(s) are not in the CAS");
    let [detail] = status.details.as_slice() else {
        panic!("not exactly one detail: {:?}", status.details);
    };
    assert_eq!(detail.type_url, PRECONDITION_FAILURE);
    let failure = PreconditionFailure::decode(detail.value.as_slice()).expect("decodes");
    assert_eq!(failure.violations, violations);

    assert_eq!(cell.cached(&job.action).await, Err(Code::NotFound));
    daemon.no_work().await;
}

/// Catches every FAILED_PRECONDITION answered as a missing input: the daemon's "no
/// driver serves the lease kind" carries no detail, and one whose violations are not
/// all `MISSING` is not the client's blob to upload. Both stay the farm's failure,
/// INTERNAL, as before. Also catches either written to the action cache.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_failed_precondition_stays_the_farms_failure() {
    let cell = Cell::start().await;
    let mut daemon = cell.daemon("node-precondition", 4, 8).await;
    let mixed = |blob: &Blob| {
        let violations = vec![
            missing(blob),
            Violation {
                r#type: "STALE".to_owned(),
                ..missing(blob)
            },
        ];
        vec![any(
            PRECONDITION_FAILURE,
            PreconditionFailure { violations }.encode_to_vec(),
        )]
    };
    for name in ["no detail", "mixed violations"] {
        let job = Job::new(name, &[]);
        cell.upload(&job.blobs()).await;
        let mut ops = cell.execute(&job.action).await;
        let start = daemon.start().await;
        let mut report = failed(start.lease_id, Code::FailedPrecondition);
        if name == "mixed violations" {
            report.status.as_mut().expect("a status").details = mixed(&job.input);
        }
        assert!(daemon.report(report).await.accepted, "{name}");
        let status = response(&done(&mut ops).await).status.expect("a status");
        assert_eq!(status.code, Code::Internal as i32, "{name}");
        assert_eq!(status.details, Vec::new(), "{name}");
        assert_eq!(
            cell.cached(&job.action).await,
            Err(Code::NotFound),
            "{name}"
        );
    }
}
