//! The bytes on the wire against `tests/golden/`: the messages and the refusal details as
//! `js-proto` at the pinned revision writes them (copied from its own goldens where it has
//! them), the trailers of each refusal, and what the client sends.

mod support;

use std::collections::BTreeMap;

use bytes::Bytes;
use http::{HeaderMap, HeaderValue};
use js_proto::refusal::{refusal_of, refusal_status};
use js_proto::v1::{
    AttemptOutcome, CallbackState, GetJobResponse, GetResultResponse, Job, JobResult, JobSpec,
    JobState, RefusalCode, SubmitJobRequest,
};
use kbf_job_client::Error;
use kbf_job_client::fake::{Call, Fault};
use prost::Message;
use prost::encoding::{WireType, encode_key, encode_varint};
use support::{
    called_back_job, fake, full_job, full_result, full_spec, golden_bytes, golden_callback,
    golden_text, retried_job,
};
use tonic::{Code, Status};

const REFUSALS: [RefusalCode; 4] = [
    RefusalCode::JobIdConflict,
    RefusalCode::NoCapableHost,
    RefusalCode::WallTimeNotPositive,
    RefusalCode::WallTimeAboveCeiling,
];

/// Asserts `value` encodes to the golden `name`, and the golden decodes to `value` and
/// encodes back to itself.
fn round_trips<M: Message + Default + PartialEq + std::fmt::Debug>(name: &str, value: &M) {
    let golden = golden_bytes(name);
    assert_eq!(
        value.encode_to_vec(),
        golden,
        "{name}: what js-proto writes"
    );
    let decoded = M::decode(golden.as_slice()).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert_eq!(&decoded, value, "{name}: what the golden decodes to");
    assert_eq!(decoded.encode_to_vec(), golden, "{name}: encoded again");
}

/// Asserts that each golden decodes with the generated types and encodes back to the same
/// bytes, and equals what the pinned `js-proto` writes for the value the test builds: a
/// fully populated `SubmitJobRequest`, `Job` and `JobResult`, and the details of the
/// `job-id-conflict` refusal. Catches a raise of the jobscheduler revision that changes the
/// wire without the goldens being reviewed, and a field dropped on the way.
#[test]
fn golden_bodies_equal_what_the_pinned_js_proto_writes() {
    let request = SubmitJobRequest {
        spec: Some(full_spec("presigned-url-text")),
    };
    round_trips("submit_job_request.hex", &request);
    round_trips::<Job>("job.hex", &full_job());
    round_trips::<JobResult>("job_result.hex", &full_result());
    let details = refusal_status(RefusalCode::JobIdConflict, "job-1 names another job");
    assert_eq!(details.details(), golden_bytes("refusal_status.hex"));
}

/// The bytes of the golden `base` followed by those of the golden `added`: a message with the
/// fields of `added` set encodes as the message without them, then those fields.
fn appended(base: &[u8], added: &str) -> Vec<u8> {
    let mut bytes = base.to_vec();
    bytes.extend(golden_bytes(added));
    bytes
}

/// Asserts the goldens of the fields the retry and callback work added, as the pinned
/// `js-proto` writes them: a job with its attempts (`job_retry.hex`) and with its callback's
/// delivery status (`job_callback.hex`), each after the bytes of `job.hex`, and a spec with a
/// callback (`spec_callback.hex`) after the bytes of the golden submit's spec. Each decodes to
/// the value the test builds, the attempts in order and each outcome typed. Catches a raise of
/// the jobscheduler revision that renumbers or re-types a field kbf reads, and a field that
/// loses its presence (a delivered time of 0, a deadline of 0).
#[test]
fn the_attempt_and_callback_goldens_equal_what_the_pinned_js_proto_writes() {
    let job = golden_bytes("job.hex");
    let retried = appended(&job, "job_retry.hex");
    assert_eq!(retried_job().encode_to_vec(), retried);
    let read = Job::decode(retried.as_slice()).unwrap();
    assert_eq!(read, retried_job());
    let outcomes: Vec<_> = read.attempt_history.iter().map(|a| a.outcome()).collect();
    assert_eq!(
        outcomes,
        [
            AttemptOutcome::StartFailed,
            AttemptOutcome::HostLost,
            AttemptOutcome::Failed
        ]
    );
    assert_eq!((read.attempt, read.dropped_attempts), (3, 1));

    let called = appended(&job, "job_callback.hex");
    assert_eq!(called_back_job().encode_to_vec(), called);
    let read = Job::decode(called.as_slice()).unwrap();
    assert_eq!(read, called_back_job());
    let status = read.callback_status.unwrap();
    assert_eq!(status.state(), CallbackState::Pending);
    assert_eq!(status.delivered_at_ms, Some(0));

    let spec = full_spec("presigned-url-text").encode_to_vec();
    let with_callback = appended(&spec, "spec_callback.hex");
    let asked = JobSpec {
        callback: Some(golden_callback()),
        ..full_spec("presigned-url-text")
    };
    assert_eq!(asked.encode_to_vec(), with_callback);
    assert_eq!(JobSpec::decode(with_callback.as_slice()).unwrap(), asked);
    // The previous goldens are of a job and a spec without these fields: a read of them gives
    // attempt 0 (which reads as 1), no history and no callback.
    let old = Job::decode(job.as_slice()).unwrap();
    assert_eq!(
        (old.attempt, old.attempt_history.len(), old.callback_status),
        (0, 0, None)
    );
}

/// Asserts that a job with attempts and one with a callback status, served by the fake as a
/// `GetJob` answer, come out of the client unchanged. Catches a client that drops the fields
/// of a retried or called-back job on the way in.
#[tokio::test]
async fn attempts_and_callback_status_come_through_the_client_unchanged() {
    let (server, client) = fake().await;
    for job in [retried_job(), called_back_job()] {
        let answer = GetJobResponse {
            job: Some(job.clone()),
        };
        server.inject(Fault::Answer(answer.encode_to_vec().into()));
        assert_eq!(client.get_job("job-1").await.unwrap(), job);
    }
}

/// One refusal of `refusal_trailers.txt`: its name, the detail it was made with, and the
/// three trailers.
struct Trailers {
    name: String,
    detail: String,
    headers: BTreeMap<String, String>,
}

fn trailers() -> Vec<Trailers> {
    let text = golden_text("refusal_trailers.txt");
    let mut blocks: Vec<Trailers> = Vec::new();
    for line in text.lines().filter(|line| !line.starts_with('#')) {
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            blocks.push(Trailers {
                name: name.to_owned(),
                detail: String::new(),
                headers: BTreeMap::new(),
            });
        } else if let Some((key, value)) = line.split_once(": ") {
            let block = blocks.last_mut().expect("a line inside a block");
            match key {
                "detail" => block.detail = value.to_owned(),
                header => {
                    block.headers.insert(header.to_owned(), value.to_owned());
                }
            }
        }
    }
    blocks
}

/// Asserts the four refusal trailers: each golden block is what `refusal_status` writes into
/// the trailers for its code and detail (`grpc-status`, `grpc-message`,
/// `grpc-status-details-bin`), and the trailers read back through `refusal_of` give that
/// code and detail; a status with the same code and text but no details is no refusal, and
/// the client answers it `Error::Status`, not `Error::Refused`. Catches a client that reads
/// the message text instead of the details, and a change of the trailers' encoding.
#[tokio::test]
async fn golden_refusals_decode_to_their_codes() {
    let blocks = trailers();
    let names: Vec<&str> = blocks.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names, REFUSALS.map(RefusalCode::as_str));
    for (block, code) in blocks.iter().zip(REFUSALS) {
        let mut written = HeaderMap::new();
        refusal_status(code, block.detail.clone())
            .add_header(&mut written)
            .unwrap();
        let written: BTreeMap<String, String> = written
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_owned()))
            .collect();
        assert_eq!(written, block.headers, "{}", block.name);

        let mut read = HeaderMap::new();
        for (key, value) in &block.headers {
            let key = http::HeaderName::from_bytes(key.as_bytes()).unwrap();
            read.insert(key, HeaderValue::from_str(value).unwrap());
        }
        let status = Status::from_header_map(&read).unwrap();
        let refusal = refusal_of(&status).unwrap();
        assert_eq!(
            (refusal.code(), refusal.detail.as_str()),
            (code, block.detail.as_str())
        );
        assert_eq!(status.code(), code.grpc_code());
    }

    let (server, client) = fake().await;
    let conflict = &blocks[0];
    server.inject(Fault::Status {
        code: Code::AlreadyExists,
        message: conflict.detail.clone(),
    });
    let error = client.submit_job(full_spec("x")).await.unwrap_err();
    assert!(
        matches!(&error, Error::Status { code: Code::AlreadyExists, message } if *message == conflict.detail),
        "{error:?}"
    );
    assert!(!error.is(RefusalCode::JobIdConflict));
}

/// Asserts that the request the fake decoded for a fully populated `JobSpec` equals the one
/// the test built, field for field, and encodes to the golden submit. Catches a client that
/// drops, reorders or renames a field on the way out.
#[tokio::test]
async fn what_the_client_sends_is_what_the_server_reads() {
    let (server, client) = fake().await;
    let spec = full_spec("presigned-url-text");
    client.submit_job(spec.clone()).await.unwrap();
    let sent = SubmitJobRequest { spec: Some(spec) };
    assert_eq!(server.calls(), [Call::SubmitJob(Box::new(sent.clone()))]);
    assert_eq!(sent.encode_to_vec(), golden_bytes("submit_job_request.hex"));
}

/// Appends two fields no message of the service has (numbers over 100000), one varint and
/// one length-delimited, to `buf`.
fn add_unknown_fields(buf: &mut Vec<u8>) {
    encode_key(100_000, WireType::Varint, buf);
    encode_varint(7, buf);
    encode_key(100_001, WireType::LengthDelimited, buf);
    encode_varint(3, buf);
    buf.extend_from_slice(b"abc");
}

/// `message` as field 1 of a response, with unknown fields after it inside the message
/// (when `inside`) and after it in the response.
fn response_with_unknown_fields(message: &[u8], inside: bool) -> Bytes {
    let mut inner = message.to_vec();
    if inside {
        add_unknown_fields(&mut inner);
    }
    let mut out = Vec::new();
    encode_key(1, WireType::LengthDelimited, &mut out);
    encode_varint(inner.len() as u64, &mut out);
    out.extend_from_slice(&inner);
    add_unknown_fields(&mut out);
    out.into()
}

/// Asserts that fully populated answers served by the fake come out of the client unchanged:
/// a `GetJobResponse` with `full_job()` gives exactly `full_job()`; a `GetResultResponse`
/// with `full_result()` (usage populated, exit code 3) gives exactly `full_result()`; a result
/// whose exit code is `Some(0)` keeps `Some(0)`. Catches a client that drops or rewrites a
/// field of the answer, and an optional zero read as absent.
#[tokio::test]
async fn full_answers_come_through_the_client_unchanged() {
    let (server, client) = fake().await;
    server.inject(Fault::Answer(
        GetJobResponse {
            job: Some(full_job()),
        }
        .encode_to_vec()
        .into(),
    ));
    assert_eq!(client.get_job("job-1").await.unwrap(), full_job());

    server.inject(Fault::Answer(
        GetResultResponse {
            result: Some(full_result()),
        }
        .encode_to_vec()
        .into(),
    ));
    let result = client.get_result("job-1").await.unwrap();
    assert_eq!(result, full_result());
    assert_eq!(result.exit_code, Some(3));
    assert!(result.usage.is_some());

    let zero = JobResult {
        exit_code: Some(0),
        ..full_result()
    };
    server.inject(Fault::Answer(
        GetResultResponse {
            result: Some(zero.clone()),
        }
        .encode_to_vec()
        .into(),
    ));
    let read = client.get_result("job-1").await.unwrap();
    assert_eq!(read.exit_code, Some(0));
    assert_eq!(read, zero);
}

/// Asserts that fields a newer server adds (unknown numbers, varint and length-delimited,
/// inside the job or result and beside it) are skipped: the client returns the same value as
/// without them. Catches a client that refuses an answer for fields it does not know.
#[tokio::test]
async fn unknown_fields_in_an_answer_are_skipped() {
    let (server, client) = fake().await;
    for inside in [false, true] {
        server.inject(Fault::Answer(response_with_unknown_fields(
            &full_job().encode_to_vec(),
            inside,
        )));
        assert_eq!(client.get_job("job-1").await.unwrap(), full_job());

        server.inject(Fault::Answer(response_with_unknown_fields(
            &full_result().encode_to_vec(),
            inside,
        )));
        assert_eq!(client.get_result("job-1").await.unwrap(), full_result());
    }
}

/// Asserts that a job whose state is a number this client does not know (42) is returned with
/// that number in `state` and `JobState::Unspecified` from `state()`. Catches an answer
/// refused or the number changed because a newer server added a state.
#[tokio::test]
async fn an_unknown_job_state_is_kept_as_its_number() {
    let (server, client) = fake().await;
    let newer = Job {
        state: 42,
        ..full_job()
    };
    server.inject(Fault::Answer(
        GetJobResponse {
            job: Some(newer.clone()),
        }
        .encode_to_vec()
        .into(),
    ));
    let job = client.get_job("job-1").await.unwrap();
    assert_eq!(job.state, 42);
    assert_eq!(job.state(), JobState::Unspecified);
    assert_eq!(job, newer);
}
