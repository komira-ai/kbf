//! A stock S3 SDK (aws-sdk-rust) against the S3 front over loopback, configured as the
//! "Using a stock SDK" section of `packaging/tierstore/README.md` says: the other tests of the
//! front sign their requests by hand or with the client of kbf, so these are the ones that show
//! what a client nobody here wrote sends and gets back.

use std::fmt::Debug;

use aws_sdk_s3::config::http::HttpResponse;
use aws_sdk_s3::config::{
    Builder, Credentials, Region, RequestChecksumCalculation, ResponseChecksumValidation,
};
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart, EncodingType};
use aws_sdk_s3::{Client, Config};
use tierstore::server::AccessKey;

use crate::files::Files;
use crate::support::{BUCKET, Harness, KEY_ID, SECRET};

/// The id and the secret of a read-only key.
const READER: (&str, &str) = ("AKIDSDKREADER", "sdk-reader-secret");

/// The settings the front needs of a client, for the key `(id, secret)` of a server.
fn settings(h: &Harness, key: (&str, &str)) -> Builder {
    Config::builder()
        .behavior_version_latest()
        // The SDK's own HTTP client, without TLS: the front serves plain HTTP.
        .http_client(aws_smithy_http_client::Builder::new().build_http())
        .endpoint_url(format!("http://{}", h.addr))
        .force_path_style(true)
        .region(Region::new("us-east-1"))
        .credentials_provider(Credentials::new(key.0, key.1, None, None, "static"))
        // No flexible checksums unless an operation requires one: none of the calls here does,
        // so no checksum header or trailer is sent, and no aws-chunked body.
        .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
        .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
}

/// An SDK client with the key of the server.
fn client(h: &Harness) -> Client {
    Client::from_conf(settings(h, (KEY_ID, SECRET)).build())
}

/// An SDK client with a read-only key.
fn reader(h: &Harness) -> Client {
    Client::from_conf(settings(h, READER).build())
}

/// A server with one read-only key, that tells the time by the clock of the machine, as the
/// SDK does.
async fn serving() -> Harness {
    Harness::start(|config| config.read_only_keys = vec![AccessKey::new(READER.0, READER.1)]).await
}

/// The HTTP status and the S3 error code of a call the server refused.
fn refusal<T: Debug, E: ProvideErrorMetadata + Debug>(
    result: Result<T, SdkError<E, HttpResponse>>,
) -> (u16, String) {
    let error = result.unwrap_err();
    let status = error.raw_response().expect("an answer").status().as_u16();
    (status, error.code().unwrap_or_default().to_owned())
}

/// Puts `body` under `name` with `If-None-Match: *`, as a create-only client does.
async fn put(client: &Client, name: &str, body: &[u8]) -> Result<(), (u16, String)> {
    let sent = client
        .put_object()
        .bucket(BUCKET)
        .key(name)
        .if_none_match("*")
        .body(ByteStream::from(body.to_vec()))
        .send()
        .await;
    match sent {
        Ok(_) => Ok(()),
        Err(error) => Err(refusal::<(), _>(Err(error))),
    }
}

/// The body of `name`, whole or by `range`.
async fn get(client: &Client, name: &str, range: Option<&str>) -> Vec<u8> {
    let out = client
        .get_object()
        .bucket(BUCKET)
        .key(name)
        .set_range(range.map(str::to_owned))
        .send()
        .await
        .unwrap();
    out.body.collect().await.unwrap().into_bytes().to_vec()
}

/// The keys of a page of a listing, whether it is cut off, and the token of the next page.
async fn page(
    client: &Client,
    prefix: &str,
    max_keys: i32,
    token: Option<&str>,
) -> (Vec<String>, bool, Option<String>) {
    let out = client
        .list_objects_v2()
        .bucket(BUCKET)
        .prefix(prefix)
        .max_keys(max_keys)
        .set_continuation_token(token.map(str::to_owned))
        .send()
        .await
        .unwrap();
    let keys = out
        .contents()
        .iter()
        .map(|object| object.key().unwrap().to_owned())
        .collect();
    let more = out.is_truncated().unwrap_or(false);
    (keys, more, out.next_continuation_token().map(str::to_owned))
}

/// Asserts that the SDK puts an object, gets it whole and by ranges, heads it, and deletes it, and
/// that a missing key reads as the SDK's own `NoSuchKey` and `NotFound`: the calls that every
/// output of a job goes through, with the headers and the signature of a client the front's
/// authors did not write.
#[tokio::test]
async fn sdk_puts_gets_heads_and_deletes_an_object() {
    let h = serving().await;
    let sdk = client(&h);
    put(&sdk, "out/a", b"hello world").await.unwrap();
    assert_eq!(get(&sdk, "out/a", None).await, b"hello world");
    assert_eq!(get(&sdk, "out/a", Some("bytes=6-10")).await, b"world");
    assert_eq!(get(&sdk, "out/a", Some("bytes=6-")).await, b"world");
    assert_eq!(get(&sdk, "out/a", Some("bytes=-5")).await, b"world");
    let ranged = sdk
        .get_object()
        .bucket(BUCKET)
        .key("out/a")
        .range("bytes=0-4")
        .send()
        .await
        .unwrap();
    assert_eq!(ranged.content_range(), Some("bytes 0-4/11"));
    let tag = ranged.e_tag().expect("a tag").to_owned();
    let head = sdk
        .head_object()
        .bucket(BUCKET)
        .key("out/a")
        .send()
        .await
        .unwrap();
    assert_eq!(head.content_length(), Some(11));
    assert_eq!(head.e_tag(), Some(tag.as_str()));
    assert!(head.last_modified().is_some());
    let missing = sdk
        .head_object()
        .bucket(BUCKET)
        .key("out/none")
        .send()
        .await;
    assert!(missing.unwrap_err().into_service_error().is_not_found());
    let missing = sdk.get_object().bucket(BUCKET).key("out/none").send().await;
    assert!(missing.unwrap_err().into_service_error().is_no_such_key());
    sdk.delete_object()
        .bucket(BUCKET)
        .key("out/a")
        .send()
        .await
        .unwrap();
    let gone = sdk.head_object().bucket(BUCKET).key("out/a").send().await;
    assert!(gone.unwrap_err().into_service_error().is_not_found());
    // A delete of a key that is not there succeeds, as it does in S3.
    sdk.delete_object()
        .bucket(BUCKET)
        .key("out/a")
        .send()
        .await
        .unwrap();
}

/// Asserts that a second put of a key is refused with 412 `PreconditionFailed` and the SDK
/// returns it as an error, whether the put carries `If-None-Match: *` or not, and that the
/// first body is the one that stays.
#[tokio::test]
async fn sdk_second_put_of_a_key_is_refused() {
    let h = serving().await;
    let sdk = client(&h);
    put(&sdk, "out/once", b"first").await.unwrap();
    let again = put(&sdk, "out/once", b"second").await;
    assert_eq!(again, Err((412, "PreconditionFailed".to_owned())));
    let plain = sdk
        .put_object()
        .bucket(BUCKET)
        .key("out/once")
        .body(ByteStream::from_static(b"third"))
        .send()
        .await;
    assert_eq!(refusal(plain), (412, "PreconditionFailed".to_owned()));
    assert_eq!(get(&sdk, "out/once", None).await, b"first");
}

/// Asserts that ListObjectsV2 with the SDK lists by prefix, in key order, pages by
/// `max-keys` with the continuation token the front gives, and does not show the front's own
/// keys.
#[tokio::test]
async fn sdk_lists_by_prefix_and_continues() {
    let h = serving().await;
    let sdk = client(&h);
    for name in ["a/1", "a/2", "a/3", "b/1"] {
        put(&sdk, name, name.as_bytes()).await.unwrap();
    }
    let (keys, more, token) = page(&sdk, "a/", 2, None).await;
    assert_eq!(keys, ["a/1", "a/2"]);
    assert!(more);
    let token = token.expect("a token for the next page");
    let (keys, more, token) = page(&sdk, "a/", 2, Some(&token)).await;
    assert_eq!(keys, ["a/3"]);
    assert!(!more);
    assert_eq!(token, None);
    let (keys, more, _) = page(&sdk, "", 100, None).await;
    assert_eq!(keys, ["a/1", "a/2", "a/3", "b/1"]);
    assert!(!more);
}

/// Asserts that a read-only key, used through the SDK, gets, heads and lists, that its put and its
/// delete are refused with 403 `AccessDenied`, and that the object is as it was after them.
#[tokio::test]
async fn sdk_read_only_key_reads_and_is_refused_writes() {
    let h = serving().await;
    let full = client(&h);
    let reading = reader(&h);
    put(&full, "out/kept", b"kept").await.unwrap();
    assert_eq!(get(&reading, "out/kept", None).await, b"kept");
    let head = reading.head_object().bucket(BUCKET).key("out/kept").send();
    assert_eq!(head.await.unwrap().content_length(), Some(4));
    let (keys, _, _) = page(&reading, "out/", 10, None).await;
    assert_eq!(keys, ["out/kept"]);
    let denied = (403, "AccessDenied".to_owned());
    assert_eq!(put(&reading, "out/new", b"new").await, Err(denied.clone()));
    let delete = reading
        .delete_object()
        .bucket(BUCKET)
        .key("out/kept")
        .send();
    assert_eq!(refusal(delete.await), denied);
    assert_eq!(get(&full, "out/kept", None).await, b"kept");
    let new = full
        .head_object()
        .bucket(BUCKET)
        .key("out/new")
        .send()
        .await;
    assert!(new.unwrap_err().into_service_error().is_not_found());
}

/// Asserts that the multipart flow of the SDK (create, two parts, complete) makes one object
/// that reads back whole, by a range across the join of the parts, and with its size, and that an
/// upload that is aborted is accepted: the way an output too big for one put goes up.
#[tokio::test]
async fn sdk_multipart_upload_completes() {
    let h = serving().await;
    let sdk = client(&h);
    let first = vec![1_u8; 5 * 1024 * 1024];
    let second = vec![2_u8; 1000];
    let created = sdk
        .create_multipart_upload()
        .bucket(BUCKET)
        .key("out/big")
        .send()
        .await
        .unwrap();
    let id = created.upload_id().expect("an upload id").to_owned();
    let mut parts = CompletedMultipartUpload::builder();
    for (number, body) in [(1, &first), (2, &second)] {
        let part = sdk
            .upload_part()
            .bucket(BUCKET)
            .key("out/big")
            .upload_id(&id)
            .part_number(number)
            .body(ByteStream::from(body.clone()))
            .send()
            .await
            .unwrap();
        let tag = part.e_tag().expect("a tag for the part");
        parts = parts.parts(
            CompletedPart::builder()
                .part_number(number)
                .e_tag(tag)
                .build(),
        );
    }
    sdk.complete_multipart_upload()
        .bucket(BUCKET)
        .key("out/big")
        .upload_id(&id)
        .multipart_upload(parts.build())
        .send()
        .await
        .unwrap();
    let head = sdk.head_object().bucket(BUCKET).key("out/big").send();
    assert_eq!(head.await.unwrap().content_length(), Some(5_243_880));
    let join = get(&sdk, "out/big", Some("bytes=5242878-5242881")).await;
    assert_eq!(join, [1, 1, 2, 2]);
    let aborted = sdk
        .create_multipart_upload()
        .bucket(BUCKET)
        .key("out/dropped")
        .send()
        .await
        .unwrap();
    sdk.abort_multipart_upload()
        .bucket(BUCKET)
        .key("out/dropped")
        .upload_id(aborted.upload_id().unwrap())
        .send()
        .await
        .unwrap();
    let dropped = sdk.head_object().bucket(BUCKET).key("out/dropped").send();
    assert!(
        dropped
            .await
            .unwrap_err()
            .into_service_error()
            .is_not_found()
    );
}

/// Asserts what the settings stand on: a client whose region is not the server's is refused with
/// 403 `InvalidAccessKeyId`, because the region is part of what the signature covers, and a
/// listing that asks for `encoding-type=url`, which some SDKs send by default, is 501
/// `NotImplemented` (the Rust SDK sends it only when asked to).
#[tokio::test]
async fn sdk_region_and_encoding_type_are_what_the_front_refuses() {
    let h = serving().await;
    let elsewhere = settings(&h, (KEY_ID, SECRET)).region(Region::new("eu-west-1"));
    let sdk = Client::from_conf(elsewhere.build());
    let listed = sdk.list_objects_v2().bucket(BUCKET).send().await;
    assert_eq!(refusal(listed), (403, "InvalidAccessKeyId".to_owned()));
    let encoded = client(&h)
        .list_objects_v2()
        .bucket(BUCKET)
        .encoding_type(EncodingType::Url)
        .send()
        .await;
    assert_eq!(refusal(encoded), (501, "NotImplemented".to_owned()));
}

/// Asserts why the settings turn flexible checksums off: with the SDK's defaults, a put whose
/// body is a file, which the SDK sends with a trailing checksum in an aws-chunked body, is
/// refused with 501 `NotImplemented`, and nothing is stored.
#[tokio::test]
async fn sdk_default_checksums_on_a_streamed_body_are_refused() {
    let h = serving().await;
    let files = Files::new("sdk-defaults");
    let defaults = Config::builder()
        .behavior_version_latest()
        .http_client(aws_smithy_http_client::Builder::new().build_http())
        .endpoint_url(format!("http://{}", h.addr))
        .force_path_style(true)
        .region(Region::new("us-east-1"))
        .credentials_provider(Credentials::new(KEY_ID, SECRET, None, None, "static"));
    let sdk = Client::from_conf(defaults.build());
    let body = ByteStream::from_path(&files.key).await.unwrap();
    let sent = sdk
        .put_object()
        .bucket(BUCKET)
        .key("out/streamed")
        .body(body)
        .send()
        .await;
    assert_eq!(refusal(sent), (501, "NotImplemented".to_owned()));
    let kept = client(&h)
        .head_object()
        .bucket(BUCKET)
        .key("out/streamed")
        .send()
        .await;
    assert!(kept.unwrap_err().into_service_error().is_not_found());
}
