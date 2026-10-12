//! The REAPI `RequestMetadata` header: a header captured from a real buck2 build, and
//! bad headers on real gRPC calls.

mod common;

use std::sync::{Arc, Mutex};

use base64::Engine as _;
use common::{Blob, Farm};
use kbf_front::{CallMetadata, Read, RequestMetadataLayer};
use kbf_proto::reapi::{FindMissingBlobsRequest, RequestMetadata};
use prost::Message as _;
use tonic::codegen::http;
use tonic::metadata::MetadataMap;

/// The captured header (`tests/data/buck2-execute.requestmetadata`): its name and its
/// value as they arrived.
fn golden() -> (String, String) {
    let file = include_str!("data/buck2-execute.requestmetadata");
    let line = file
        .lines()
        .find(|l| !l.starts_with('#'))
        .expect("a header line");
    let (name, value) = line.split_once(": ").expect("name: value");
    (name.to_owned(), value.to_owned())
}

/// Catches: reading another header than the one buck2 sends, and reading the id from
/// another field number than buck2 writes it in (field 2, `action_id`, for 3: the
/// prost round trip cannot see that, because both of its sides use the same
/// generated code). The UUID is the `Build ID` that build printed.
#[test]
fn the_header_buck2_sent_is_read_with_its_build_id() {
    let (name, value) = golden();
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::HeaderName::from_bytes(name.as_bytes()).expect("a header name"),
        http::HeaderValue::from_str(&value).expect("a header value"),
    );
    let Read::Valid(read) = CallMetadata::from_headers(&headers) else {
        panic!("the captured header was not read");
    };
    assert_eq!(
        read.invocation_display(),
        "3136aef1-b380-4ac0-ac06-c6d6bbb65842"
    );
    assert_eq!(
        (
            read.tool_name.value.as_str(),
            read.tool_version.value.as_str()
        ),
        ("buck2", "0.1.0")
    );
    // What the pinned buck2 leaves empty (docs/design/failure-classes.md, O2).
    for empty in [
        &read.action_id,
        &read.correlated_invocations_id,
        &read.action_mnemonic,
        &read.target_id,
        &read.configuration_id,
    ] {
        assert_eq!(empty.value, "");
    }
}

/// Where the test's subscriber writes.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Log {
    fn lines_with(&self, part: &str) -> usize {
        String::from_utf8_lossy(&self.0.lock().expect("log"))
            .lines()
            .filter(|l| l.contains(part) && l.contains("kbf_invocation_metadata_invalid"))
            .count()
    }
}

/// Catches: a layer that refuses a call whose header is too large, does not decode or
/// carries an id kbf does not accept (bookkeeping breaking a build); one that does not
/// count them, or counts a good header or a call with none; and one that does not log
/// the call and why its header was treated as absent.
#[tokio::test]
async fn a_bad_header_never_fails_a_call_and_is_counted() {
    // The server runs on this test's one thread, so the thread's subscriber sees it.
    let log = Log::default();
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let layer = RequestMetadataLayer::new();
    let farm = Farm::with_request_metadata(layer.clone()).await;
    let blob = Blob::new("x");
    let (_, good) = golden();
    let base64 = |bytes: &[u8]| base64::engine::general_purpose::STANDARD_NO_PAD.encode(bytes);
    let bad_id = RequestMetadata {
        tool_invocation_id: "a~b".to_owned(),
        ..Default::default()
    };
    let not_accepted = "the tool_invocation_id is not an id kbf accepts";
    let undecodable = "the header does not decode";
    let cases = [
        ("no header", None, None),
        ("the captured header", Some(good), None),
        (
            "an id with ~",
            Some(base64(&bad_id.encode_to_vec())),
            Some(not_accepted),
        ),
        (
            "not base64",
            Some("not base64!".to_owned()),
            Some(undecodable),
        ),
        (
            "not a RequestMetadata",
            Some(base64(&[0x0f, 0])),
            Some(undecodable),
        ),
        (
            "over 8 KiB",
            Some(base64(&vec![0; kbf_front::MAX_HEADER_BYTES + 1])),
            Some("the header is over 8192 bytes"),
        ),
    ];
    let mut expected = 0;
    for (what, value, reason) in cases {
        let mut headers = http::HeaderMap::new();
        if let Some(value) = value {
            let value = http::HeaderValue::from_str(&value).expect("a header value");
            headers.insert(kbf_front::REQUEST_METADATA_HEADER, value);
        }
        let mut request = tonic::Request::new(FindMissingBlobsRequest {
            blob_digests: vec![blob.proto.clone()],
            ..Default::default()
        });
        *request.metadata_mut() = MetadataMap::from_headers(headers);
        let answer = farm
            .cas()
            .find_missing_blobs(request)
            .await
            .unwrap_or_else(|e| panic!("{what}: the call failed: {e}"));
        assert_eq!(answer.into_inner().missing_blob_digests.len(), 1, "{what}");
        expected += u64::from(reason.is_some());
        assert_eq!(layer.invalid_count(), expected, "{what}");
    }
    let call = "/build.bazel.remote.execution.v2.ContentAddressableStorage/FindMissingBlobs";
    assert_eq!(log.lines_with(call), 4);
    assert_eq!(
        [not_accepted, undecodable, "over 8192"].map(|r| log.lines_with(r)),
        [1, 2, 1]
    );
}
