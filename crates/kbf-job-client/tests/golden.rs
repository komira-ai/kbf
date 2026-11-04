//! The bytes on the wire against `tests/golden/`: the messages and the refusal details as
//! `js-proto` at the pinned revision writes them (copied from its own goldens where it has
//! them), the trailers of each refusal, and what the client sends.

mod support;

use std::collections::BTreeMap;

use http::{HeaderMap, HeaderValue};
use js_proto::refusal::{refusal_of, refusal_status};
use js_proto::v1::{Job, JobResult, RefusalCode, SubmitJobRequest};
use kbf_job_client::Error;
use kbf_job_client::fake::{Call, Fault};
use prost::Message;
use support::{fake, full_job, full_result, full_spec, golden_bytes, golden_text};
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
