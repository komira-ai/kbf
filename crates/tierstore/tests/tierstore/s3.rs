//! The S3 backend over the loopback S3 service of `kbf-objstore`: the checks every backend
//! passes, and the cases only S3 has (ranges that S3 answers 416 to, listings of more than one
//! page, names the client cannot sign, a store that cannot be reached, the proxy and the
//! reconciler working over S3 backends).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::{StreamExt, stream};
use kbf_objstore::s3::fake::{Failure, FakeS3, part_etag};
use tierstore::s3::{Capabilities, Credentials, S3Config};
use tierstore::{
    Backend, ByteRange, Check, Clock, Error, PartBody, Proxy, ProxyConfig, Reconciler,
    ReconcilerConfig, S3Backend, SelfTestError, SelfTestOptions, SystemClock, self_test,
};

use crate::conformance;
use crate::list_uploads::contract as uploads_contract;
use crate::multipart::{contract, large};
use crate::support::{body, keys, page};

fn config(endpoint: &str) -> S3Config {
    S3Config {
        endpoint: endpoint.to_owned(),
        region: "us-east-1".to_owned(),
        bucket: "tier-test".to_owned(),
        credentials: Credentials::new("test-key", "test-secret"),
        capabilities: Capabilities::default(),
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(30),
        min_body_rate: None,
    }
}

/// A backend over a service of its own, which is returned with it.
async fn backend() -> (FakeS3, S3Backend) {
    let s3 = FakeS3::start().await;
    let backend = S3Backend::new(config(s3.endpoint())).unwrap();
    (s3, backend)
}

/// Asserts that the self-test passes over the S3 service, as a hot tier and as a cold tier (the
/// conditional puts, the range, the head, the paging and the multipart completion of the service
/// are the ones the test asks for), and leaves no object and no open upload in the bucket.
#[tokio::test]
async fn the_self_test_passes_over_the_s3_service_and_leaves_nothing() {
    let (s3, backend) = backend().await;
    for multipart in [false, true] {
        let options = self_test_options("run-1", multipart);
        self_test(&backend, &options, &|line: String| panic!("{line}"))
            .await
            .unwrap();
    }
    assert!(s3.keys().is_empty(), "{:?}", s3.keys());
    assert_eq!(s3.incomplete_uploads(), 0);
}

/// The options of a self-test that asks a service that does not answer once (a wait of zero); a
/// test that wants a repeat sets `wait`, and the pause is a millisecond.
fn self_test_options(run_id: &str, multipart: bool) -> SelfTestOptions<'_> {
    SelfTestOptions {
        name: "the service",
        run_id,
        multipart,
        skip: &[],
        wait: Duration::ZERO,
        pause: Duration::from_millis(1),
        max_pause: Duration::from_millis(1),
    }
}

/// Asserts that a probe key an earlier start left is deleted when the service's own clock, read
/// from the time it stamps a key the test writes, says it is over an hour old, as listed with the
/// time the service stamped it with; and that a key that is not yet an hour old stays.
#[tokio::test]
async fn the_self_test_sweeps_probe_keys_of_an_earlier_start_over_the_s3_service() {
    let (s3, backend) = backend().await;
    let left = bytes::Bytes::from_static(b"x");
    s3.set_time(1);
    backend
        .put("_tiering/selftest/killed-start/create", left.clone())
        .await
        .unwrap();
    s3.set_time(5_000);
    backend
        .put("_tiering/selftest/running-start/create", left)
        .await
        .unwrap();
    let options = self_test_options("run-1", true);

    // 3000 s on the service's clock: the first key is under an hour old.
    s3.set_time(3_000);
    self_test(&backend, &options, &|line: String| panic!("{line}"))
        .await
        .unwrap();
    assert_eq!(s3.keys().len(), 2, "{:?}", s3.keys());

    // 4000 s: the first key, written at 1 s, is over an hour old; the second, at 5000 s, is not
    // older than now.
    s3.set_time(4_000);
    self_test(&backend, &options, &|line: String| panic!("{line}"))
        .await
        .unwrap();
    assert_eq!(s3.keys(), ["_tiering/selftest/running-start/create"]);
}

/// Asserts that a 500 from the service is not a wrong answer: the cold tier's multipart check is
/// asked again after one 500 and passes with a note for the repeat; and after a 500 in every
/// attempt the tier is unreachable (a start that exits 1 and is restarted), not a violation.
#[tokio::test]
async fn a_500_from_the_service_is_retried_and_then_unreachable_not_a_violation() {
    let (s3, backend) = backend().await;
    s3.fail(Failure::Create);
    let notes = Mutex::new(Vec::new());
    let note = |line: String| notes.lock().unwrap().push(line);
    let mut options = self_test_options("run-1", true);
    options.wait = Duration::from_secs(5);
    self_test(&backend, &options, &note).await.unwrap();
    assert_eq!(
        notes.lock().unwrap().len(),
        1,
        "{:?}",
        notes.lock().unwrap()
    );

    // With a wait of zero a 500 is not asked again: the tier is unreachable at once.
    options.wait = Duration::ZERO;
    s3.fail(Failure::Create);
    let outcome = self_test(&backend, &options, &note).await;
    let Err(SelfTestError::Unreachable { error, .. }) = outcome else {
        panic!("a 500 in every attempt is not unreachable");
    };
    assert!(error.contains("starting a multipart upload"), "{error}");
    assert!(error.contains("500"), "{error}");
    assert!(s3.keys().is_empty(), "{:?}", s3.keys());
}

/// Asserts that a service that does not answer is an unreachable tier, not a wrong answer, asked
/// once for a wait of zero: the closed port of the service of a test that has stopped.
#[tokio::test]
async fn the_self_test_calls_a_service_that_does_not_answer_unreachable() {
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let backend = S3Backend::new(config(&endpoint)).unwrap();
    let notes = Mutex::new(Vec::new());
    let note = |line: String| notes.lock().unwrap().push(line);
    let options = self_test_options("run-1", true);
    let outcome = self_test(&backend, &options, &note).await;
    let Err(SelfTestError::Unreachable { backend, error }) = outcome else {
        panic!("not unreachable");
    };
    assert_eq!(backend, "the service");
    assert!(error.contains("create-only"), "{error}");
    // With a wait of zero the notes are: the time of the store that could not be read (for the
    // sweep), and the delete of the check's key that failed before the attempt and after the
    // check; no repeat.
    let notes = notes.lock().unwrap();
    let count = |text: &str| notes.iter().filter(|note| note.contains(text)).count();
    assert_eq!(
        count("could not read the time of the store"),
        1,
        "{notes:?}"
    );
    assert_eq!(count("trying again"), 0, "{notes:?}");
    assert_eq!(count("could not delete the probe key"), 2, "{notes:?}");
    assert_eq!(notes.len(), 3, "{notes:?}");
}

/// A service that answers every write (a PUT or a POST) with `status`, a whole HTTP reply, a
/// DELETE with 204 and anything else with 404. Gives the endpoint and the request line of each
/// request so far.
fn refusing_writes(status: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
    const NO_CONTENT: &str =
        "HTTP/1.1 204 No Content\r\nconnection: close\r\ncontent-length: 0\r\n\r\n";
    const NOT_FOUND: &str =
        "HTTP/1.1 404 Not Found\r\nconnection: close\r\ncontent-length: 0\r\n\r\n";
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for socket in listener.incoming() {
            let Ok(mut socket) = socket else { return };
            let mut head = Vec::new();
            let mut chunk = [0u8; 1024];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                head.extend_from_slice(&chunk[..n]);
            }
            let text = String::from_utf8_lossy(&head).into_owned();
            let line = text.lines().next().unwrap_or("").to_owned();
            // The body, if there is one, is read before the reply, so the client does not see a
            // reset while it is still sending.
            let split = text.find("\r\n\r\n").map_or(text.len(), |at| at + 4);
            let length = text
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(str::trim)
                        .map(str::to_owned)
                })
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0);
            let mut have = head.len() - split;
            while have < length {
                let n = socket.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                have += n;
            }
            log.lock().unwrap().push(line.clone());
            let reply = match line.split(' ').next() {
                Some("PUT" | "POST") => status,
                Some("DELETE") => NO_CONTENT,
                _ => NOT_FOUND,
            };
            let _ = socket.write_all(reply.as_bytes());
        }
    });
    (endpoint, seen)
}

/// Asserts that a 507 from a store, through the real S3 mapping, is a full tier and not a wrong
/// answer: the hot tier's checks that need a write are each skipped with a note (five), the test
/// passes, and nothing but writes, deletes were sent (no listing: the time of the store could not
/// be read).
#[tokio::test]
async fn a_507_from_the_store_skips_the_write_checks_through_the_s3_mapping() {
    let (endpoint, seen) = refusing_writes(
        "HTTP/1.1 507 Insufficient Storage\r\nconnection: close\r\ncontent-length: 0\r\n\r\n",
    );
    let backend = S3Backend::new(config(&endpoint)).unwrap();
    let notes = Mutex::new(Vec::new());
    let note = |line: String| notes.lock().unwrap().push(line);
    let options = self_test_options("run-1", false);
    self_test(&backend, &options, &note).await.unwrap();
    let notes = notes.lock().unwrap();
    let full = notes
        .iter()
        .filter(|note| note.contains("the tier is full"));
    assert_eq!(full.count(), 5, "{notes:?}");
    let seen = seen.lock().unwrap();
    assert!(seen.iter().all(|line| !line.starts_with("GET")), "{seen:?}");
}

/// Asserts that a 501 from a store, through the real S3 mapping, is a wrong answer that is not
/// asked again: the self-test is refused as the create-only check after one put of the key, and
/// no repeat is noted.
#[tokio::test]
async fn a_501_from_the_store_is_a_violation_asked_once() {
    let (endpoint, seen) = refusing_writes(
        "HTTP/1.1 501 Not Implemented\r\nconnection: close\r\ncontent-length: 0\r\n\r\n",
    );
    let backend = S3Backend::new(config(&endpoint)).unwrap();
    let notes = Mutex::new(Vec::new());
    let note = |line: String| notes.lock().unwrap().push(line);
    // A wait of 5 s would allow a repeat, if a 501 were taken for a store that is down.
    let mut options = self_test_options("run-1", true);
    options.wait = Duration::from_secs(5);
    let outcome = self_test(&backend, &options, &note).await;
    let Err(SelfTestError::Violation(failure)) = outcome else {
        panic!("a 501 is a violation");
    };
    assert_eq!(failure.check, Check::CreateOnly);
    assert!(failure.observed.contains("501"), "{failure}");
    let notes = notes.lock().unwrap();
    assert_eq!(
        notes
            .iter()
            .filter(|note| note.contains("trying again"))
            .count(),
        0,
        "{notes:?}"
    );
    let seen = seen.lock().unwrap();
    let puts = seen
        .iter()
        .filter(|line| line.starts_with("PUT") && line.contains("/create"));
    assert_eq!(puts.count(), 1, "{seen:?}");
}

/// Asserts that a call failed with `Error::Other`.
fn assert_other<T: std::fmt::Debug>(result: Result<T, Error>) {
    assert!(matches!(result, Err(Error::Other(_))), "{result:?}");
}

/// A 416 to a ranged read.
const REFUSED: &str = "HTTP/1.1 416 Range Not Satisfiable\r\nconnection: close\r\n\
                       content-length: 0\r\n\r\n";

/// The answer to a HEAD of an object of ten bytes.
const TEN_BYTES: &str = "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: 10\r\n\
                         etag: \"t\"\r\nlast-modified: Sun, 06 Nov 1994 08:49:37 GMT\r\n\r\n";

/// A page of a listing that has more pages after it, and whose next token is always `t`.
const TRUNCATED: &str = "HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n\
                         <ListBucketResult><IsTruncated>true</IsTruncated>\
                         <NextContinuationToken>t</NextContinuationToken></ListBucketResult>";

/// Serves `answers` in order, one per connection, to requests that carry no body. Gives the
/// endpoint and the head of each request so far.
fn scripted(answers: Vec<&'static str>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for answer in answers {
            let (mut socket, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut chunk = [0u8; 1024];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut chunk).unwrap();
                assert!(n > 0, "the client closed before it sent a request");
                head.extend_from_slice(&chunk[..n]);
            }
            let text = String::from_utf8_lossy(&head).into_owned();
            log.lock().unwrap().push(text);
            socket.write_all(answer.as_bytes()).unwrap();
        }
    });
    (endpoint, seen)
}

/// Asserts that a second put of a key is refused with `AlreadyExists` and leaves the first
/// object's bytes in place.
#[tokio::test]
async fn a_second_put_of_a_key_is_refused_and_changes_nothing() {
    let (_s3, backend) = backend().await;
    conformance::a_second_put_is_refused(&backend).await;
}

/// Asserts that ranges are byte-exact: the first byte, the last byte, a slice in the middle,
/// a range running past the end (cut at the end, even when its end overflows a number), empty
/// ranges inside the object and at its end, and an error for a range starting past the end.
#[tokio::test]
async fn ranges_are_byte_exact() {
    let (_s3, backend) = backend().await;
    conformance::ranges_are_byte_exact(&backend).await;
}

/// Asserts that head gives the size and a content tag that is equal for equal bytes and
/// different for different bytes of the same size, and that `same_content` compares the two.
#[tokio::test]
async fn head_gives_the_size_and_a_tag_of_the_content() {
    let (_s3, backend) = backend().await;
    conformance::head_gives_the_size_and_a_tag_of_the_content(&backend).await;
}

/// Asserts that reading or heading an absent key gives `NotFound`, deleting it succeeds, and
/// a listing of the empty bucket is empty.
#[tokio::test]
async fn an_absent_key_is_not_found_and_deleting_it_succeeds() {
    let (_s3, backend) = backend().await;
    conformance::an_absent_key_is_not_found_and_deleting_it_succeeds(&backend).await;
}

/// Asserts that a delete removes the object and that a deleted key can be put again.
#[tokio::test]
async fn a_deleted_key_is_gone_and_can_be_put_again() {
    let (_s3, backend) = backend().await;
    conformance::a_deleted_key_is_gone_and_can_be_put_again(&backend).await;
}

/// Asserts that a listing holds the keys under the prefix, in key order, with their sizes.
#[tokio::test]
async fn a_listing_holds_the_keys_under_a_prefix_in_order() {
    let (_s3, backend) = backend().await;
    conformance::a_listing_holds_the_keys_under_a_prefix_in_order(&backend).await;
}

/// Asserts that a page of a listing holds at most `max_keys` keys that sort after the
/// start-after key, in key order, that a page shorter than `max_keys` is the last, and that
/// a page of none holds none.
#[tokio::test]
async fn a_page_of_a_listing_starts_after_a_key_and_is_cut_at_max_keys() {
    let (_s3, backend) = backend().await;
    conformance::a_page_starts_after_a_key_and_is_cut_at_max_keys(&backend).await;
}

/// Asserts that a put with the content tag of the object replaces it and gives the new
/// object's content tag; that a put with another tag, or with the tag the object had before,
/// is refused and changes nothing; and that a put over an absent key is `NotFound`.
#[tokio::test]
async fn a_put_if_match_replaces_only_the_object_with_that_tag() {
    let (_s3, backend) = backend().await;
    conformance::a_put_if_match_replaces_only_the_object_with_that_tag(&backend).await;
}

/// Asserts that a put is create-only whatever the capabilities in the configuration claim:
/// the backend replaces them.
#[tokio::test]
async fn the_backend_is_create_only_whatever_the_configuration_claims() {
    let s3 = FakeS3::start().await;
    let mut claims = config(s3.endpoint());
    claims.capabilities = Capabilities {
        conditional_put: false,
        object_lock: true,
    };
    let backend = S3Backend::new(claims).unwrap();
    conformance::a_second_put_is_refused(&backend).await;
}

/// A body of these chunks, for a part.
fn chunks(texts: &[&'static str]) -> PartBody<'static> {
    let mut items = Vec::new();
    for &text in texts {
        items.push(Ok(body(text)));
    }
    stream::iter(items).boxed()
}

/// The tag the S3 service gives a part of these bytes, as the backend gives it: without the
/// quotes.
fn part_tag(bytes: &[u8]) -> String {
    part_etag(bytes).trim_matches('"').to_owned()
}

/// Asserts that a configuration the client refuses is refused by the backend, before any
/// request.
#[test]
fn a_configuration_the_client_refuses_is_refused() {
    let refused = S3Backend::new(config("https://127.0.0.1:9000")).unwrap_err();
    assert!(refused.to_string().contains("TLS"), "{refused}");
}

/// Asserts that a ranged read of a key that is not there is `NotFound`, for a range S3 is
/// asked for and for an empty one, which is answered by a HEAD.
#[tokio::test]
async fn a_range_of_an_absent_key_is_not_found() {
    let (_s3, backend) = backend().await;
    let missing = Error::NotFound("k".to_owned());
    let range = Some(ByteRange::new(0, 1));
    assert_eq!(backend.get("k", range).await.unwrap_err(), missing);
    let empty = Some(ByteRange::new(3, 0));
    assert_eq!(backend.get("k", empty).await.unwrap_err(), missing);
}

/// Asserts that a 416 to a range that holds bytes of the object is an `Other` error that says
/// the store refused it, and not an empty read.
#[tokio::test]
async fn a_refused_range_inside_the_object_is_an_error_and_not_an_empty_read() {
    let (endpoint, _) = scripted(vec![REFUSED, TEN_BYTES]);
    let backend = S3Backend::new(config(&endpoint)).unwrap();
    let range = Some(ByteRange::new(2, 3));
    let error = backend.get("k", range).await.unwrap_err();
    assert!(error.to_string().contains("refused"), "{error}");
    assert_other(Err::<(), Error>(error));
}

/// Asserts that a range running past the largest position a store reads (2^63 - 1) is sent
/// cut there, so that no `Range` names a larger position, and that a range starting there is
/// answered by a HEAD without a ranged read.
#[tokio::test]
async fn a_range_is_cut_at_the_largest_position_a_store_reads() {
    let (endpoint, seen) = scripted(vec![REFUSED, TEN_BYTES, TEN_BYTES]);
    let backend = S3Backend::new(config(&endpoint)).unwrap();
    let long = Some(ByteRange::new(4, u64::MAX));
    backend.get("k", long).await.unwrap_err();
    let late = Some(ByteRange::new(u64::MAX >> 1, 1));
    backend.get("k", late).await.unwrap_err();

    let heads = seen.lock().unwrap();
    assert_eq!(heads.len(), 3);
    let get = heads[0].to_ascii_lowercase();
    let want = "\r\nrange: bytes=4-9223372036854775806\r\n";
    assert!(get.contains(want), "{get}");
    assert!(heads[2].starts_with("HEAD "), "{}", heads[2]);
}

/// Asserts that a listing whose page repeats the continuation token it was sent ends with an
/// `Other` error after two requests, instead of asking for ever.
#[tokio::test]
async fn a_listing_that_does_not_advance_ends_with_an_error() {
    let (endpoint, seen) = scripted(vec![TRUNCATED, TRUNCATED]);
    let backend = S3Backend::new(config(&endpoint)).unwrap();
    let error = backend.list("").await.unwrap_err();
    assert!(error.to_string().contains("stalled"), "{error}");
    assert_other(Err::<(), Error>(error));
    assert_eq!(seen.lock().unwrap().len(), 2);
}

/// Asserts that the time of last write is the service's, in milliseconds, in a head and in a
/// listing.
#[tokio::test]
async fn the_time_of_last_write_is_the_services_in_milliseconds() {
    let (s3, backend) = backend().await;
    s3.set_time(1_255_369_830);
    backend.put("k", body("v")).await.unwrap();
    let head = backend.head("k").await.unwrap();
    assert_eq!(head.modified, 1_255_369_830_000);
    let listed = backend.list("").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].modified, 1_255_369_830_000);
}

/// Asserts that a listing of more objects than one S3 page holds (1000) is followed across
/// the pages: all of it, a part of it after a key, and a page of more than 1000 keys.
#[tokio::test]
async fn a_listing_of_more_than_one_s3_page_is_followed_across_pages() {
    let (_s3, backend) = backend().await;
    let names: Vec<String> = (0..1100).map(|i| format!("k{i:04}")).collect();
    for name in &names {
        backend.put(name, body("v")).await.unwrap();
    }
    assert_eq!(keys(&backend).await, names);
    assert_eq!(page(&backend, Some("k0999"), 1500).await, names[1000..]);
    assert_eq!(page(&backend, None, 1050).await, names[..1050]);
    assert_eq!(page(&backend, Some("k0001"), 2).await, ["k0002", "k0003"]);
}

/// Asserts that a key the client cannot name (it needs escaping) is refused as `Other` by
/// every call that takes one, in a prefix and in a start-after key, and that no request
/// reaches the service.
#[tokio::test]
async fn a_key_the_client_cannot_name_is_refused_before_any_request() {
    let (s3, backend) = backend().await;
    let key = "a b";
    let range = Some(ByteRange::new(0, 1));
    assert_other(backend.put(key, body("v")).await);
    assert_other(backend.put_if_match(key, body("v"), "tag").await);
    assert_other(backend.get(key, None).await);
    assert_other(backend.get(key, range).await);
    assert_other(backend.head(key).await);
    assert_other(backend.delete(key).await);
    assert_other(backend.list(key).await);
    assert_other(backend.list_page("", Some(key), 5).await);
    assert_other(backend.create_upload(key).await);
    assert_other(backend.upload_part(key, "u", 1, chunks(&["x"])).await);
    assert_other(backend.complete_upload(key, "u", &[]).await);
    assert_other(backend.abort_upload(key, "u").await);
    assert_eq!(s3.requests(), Vec::<String>::new());
}

/// Asserts that a store that cannot be reached is `Unavailable` for every call, the calls of a
/// multipart upload included (a part whose chunks are not taken is given up with the request).
#[tokio::test]
async fn a_store_that_cannot_be_reached_is_unavailable() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let backend = S3Backend::new(config(&endpoint)).unwrap();
    let range = Some(ByteRange::new(0, 1));
    let swap = backend.put_if_match("k", body("v"), "tag");
    let errors = [
        backend.put("k", body("v")).await.unwrap_err(),
        swap.await.unwrap_err(),
        backend.get("k", None).await.unwrap_err(),
        backend.get("k", range).await.unwrap_err(),
        backend.head("k").await.unwrap_err(),
        backend.delete("k").await.unwrap_err(),
        backend.list("").await.unwrap_err(),
    ];
    for error in errors {
        assert!(error.is_unavailable(), "{error}");
    }
    let created = backend.create_upload("k").await.unwrap_err();
    let sending = backend.upload_part("k", "u", 1, chunks(&["x"]));
    let sized = backend.upload_part_with_len("k", "u", 2, Some(2), chunks(&["a", "b"]));
    let parts = [(1, "t".to_owned())];
    let completed = backend.complete_upload("k", "u", &parts);
    let errors = [
        created,
        sending.await.unwrap_err(),
        sized.await.unwrap_err(),
        completed.await.unwrap_err(),
        backend.abort_upload("k", "u").await.unwrap_err(),
    ];
    for error in errors {
        assert!(error.is_unavailable(), "{error}");
    }
}

/// Asserts that a proxy over two S3 backends acknowledges a put and reads it back, that a
/// reconciler over the same two takes the lease, copies the object to the cold backend and
/// renews the lease, that a second reconciler does nothing while the first holds the lease,
/// and that it takes the lease once the first has stopped.
#[tokio::test]
async fn a_proxy_and_a_reconciler_work_over_two_s3_backends() {
    let (_hot_s3, hot) = backend().await;
    let (_cold_s3, cold) = backend().await;
    let hot: Arc<dyn Backend> = Arc::new(hot);
    let cold: Arc<dyn Backend> = Arc::new(cold);
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let mut settings = ProxyConfig::new("p1", 1000, clock.clone());
    settings.linger = Duration::from_millis(1);
    settings.usage_refresh = Duration::ZERO;
    let open = Proxy::open(hot.clone(), cold.clone(), settings);
    let proxy = open.await.unwrap();
    proxy.put("a", body("hello")).await.unwrap();
    assert_eq!(proxy.get("a", None).await.unwrap(), body("hello"));

    let settings = ReconcilerConfig::new("r1", 1000, clock.clone());
    let mut one = Reconciler::new(hot.clone(), cold.clone(), settings);
    let settings = ReconcilerConfig::new("r2", 1000, clock);
    let mut two = Reconciler::new(hot, cold.clone(), settings);
    let report = one.pass().await;
    assert!(report.held);
    assert_eq!(report.copied, 1);
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(cold.get("a", None).await.unwrap(), body("hello"));

    assert!(!two.pass().await.held);
    assert!(one.pass().await.held);
    one.stop().await;
    assert!(two.pass().await.held);
}

/// Asserts that the S3 backend does the calls of a multipart upload as the trait says, held to
/// the same checks as the in-memory and the directory backend: parts of a length that is told
/// and of one that is not, replaced by number, with the tag the service gives them; a
/// completion that names parts wrongly, or an upload that is not held, refused; a completion
/// that never replaces an object; a part whose body fails not stored; an abort; and a large
/// part sent in chunks. No upload is left open, and the two objects made of parts were
/// completed.
#[tokio::test]
async fn the_s3_backend_does_multipart_uploads() {
    let (s3, backend) = backend().await;
    s3.set_min_part_bytes(0);
    contract(&backend, part_tag).await;
    large(&backend).await;
    assert_eq!(s3.incomplete_uploads(), 0);
    assert_eq!(s3.stats().completed, 2);
}

/// Asserts that the S3 backend lists the open uploads of the service as the trait says, held to
/// the same checks as the in-memory and the directory backend (ListMultipartUploads with the
/// prefix, the page size and the two markers), that the time of a start is the service's in
/// milliseconds, and that an upload that was completed or aborted is no longer on the service's
/// list.
#[tokio::test]
async fn the_s3_backend_lists_open_uploads() {
    let (s3, backend) = backend().await;
    s3.set_time(1_255_369_830);
    let id = backend.create_upload("when").await.unwrap();
    let listed = backend.list_uploads("", None, 10).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        (listed[0].key.as_str(), listed[0].id.as_str()),
        ("when", id.as_str())
    );
    assert_eq!(listed[0].initiated, 1_255_369_830_000);
    backend.abort_upload("when", &id).await.unwrap();

    uploads_contract(&backend).await;
    assert_eq!(s3.incomplete_uploads(), 2);
}

/// Asserts that the S3 backend asks for the pages of a listing of uploads until it has the
/// number asked for or the listing ends, from the markers of each page, leaves the upload id
/// marker out where the service sends an empty one, and ends with an error, as a listing of
/// objects does, when a page names the marker it was sent.
#[tokio::test]
async fn the_s3_backend_follows_the_pages_of_a_listing_of_uploads() {
    const FIRST: &str = "HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n\
                         <ListMultipartUploadsResult><IsTruncated>true</IsTruncated>\
                         <NextKeyMarker>k1</NextKeyMarker><NextUploadIdMarker></NextUploadIdMarker>\
                         <Upload><Key>k1</Key><UploadId>u1</UploadId>\
                         <Initiated>2009-10-12T17:50:30.000Z</Initiated></Upload>\
                         </ListMultipartUploadsResult>";
    const LAST: &str = "HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n\
                        <ListMultipartUploadsResult><IsTruncated>false</IsTruncated>\
                        <Upload><Key>k2</Key><UploadId>u2</UploadId>\
                        <Initiated>2009-10-12T17:50:31.000Z</Initiated></Upload>\
                        </ListMultipartUploadsResult>";
    let (endpoint, seen) = scripted(vec![FIRST, LAST]);
    let backend = S3Backend::new(config(&endpoint)).unwrap();
    let listed = backend.list_uploads("", None, usize::MAX).await.unwrap();
    let keys: Vec<(&str, &str)> = listed
        .iter()
        .map(|upload| (upload.key.as_str(), upload.id.as_str()))
        .collect();
    assert_eq!(keys, [("k1", "u1"), ("k2", "u2")]);
    {
        let heads = seen.lock().unwrap();
        assert_eq!(heads.len(), 2);
        assert!(!heads[0].contains("key-marker"), "{}", heads[0]);
        assert!(heads[0].contains("max-uploads=1000"), "{}", heads[0]);
        assert!(heads[1].contains("key-marker=k1"), "{}", heads[1]);
        assert!(!heads[1].contains("upload-id-marker"), "{}", heads[1]);
    }

    const STUCK: &str = "HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n\
                         <ListMultipartUploadsResult><IsTruncated>true</IsTruncated>\
                         <NextKeyMarker>k</NextKeyMarker><NextUploadIdMarker>u</NextUploadIdMarker>\
                         </ListMultipartUploadsResult>";
    let (endpoint, seen) = scripted(vec![STUCK, STUCK]);
    let backend = S3Backend::new(config(&endpoint)).unwrap();
    let error = backend.list_uploads("", None, 10).await.unwrap_err();
    assert!(error.to_string().contains("stalled"), "{error}");
    assert_other(Err::<(), Error>(error));
    let heads = seen.lock().unwrap();
    assert_eq!(heads.len(), 2);
    assert!(heads[1].contains("key-marker=k"), "{}", heads[1]);
    assert!(heads[1].contains("upload-id-marker=u"), "{}", heads[1]);
}

/// Asserts that a listing of uploads that the service refuses, or answers with something that is
/// not a listing of uploads, or that names a prefix or a marker the client cannot name, is an
/// `Other` error, as the other calls of the backend give for them, and that the last of these
/// asks nothing of the service.
#[tokio::test]
async fn a_listing_of_uploads_that_fails_is_an_error() {
    let (endpoint, _) = scripted(vec!["HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n<html/>"]);
    let store = S3Backend::new(config(&endpoint)).unwrap();
    assert_other(store.list_uploads("", None, 10).await);

    let refusal = "HTTP/1.1 403 Forbidden\r\nconnection: close\r\ncontent-length: 0\r\n\r\n";
    let (endpoint, _) = scripted(vec![refusal]);
    let store = S3Backend::new(config(&endpoint)).unwrap();
    assert_other(store.list_uploads("", None, 10).await);

    let (endpoint, seen) = scripted(vec![]);
    let store = S3Backend::new(config(&endpoint)).unwrap();
    assert_other(store.list_uploads("", Some(("", None)), 10).await);
    assert_other(store.list_uploads("a//b", None, 10).await);
    assert!(seen.lock().unwrap().is_empty());
}

/// Asserts that a completion that names a part under the least size S3 takes, before the last
/// one, is refused as `InvalidPart`, and that the upload is left open.
#[tokio::test]
async fn a_completion_with_a_small_part_before_the_last_is_an_invalid_part() {
    let (s3, backend) = backend().await;
    let id = backend.create_upload("k").await.unwrap();
    let uploading = backend.upload_part("k", &id, 1, chunks(&["small"]));
    let one = uploading.await.unwrap();
    let uploading = backend.upload_part("k", &id, 2, chunks(&["tiny"]));
    let two = uploading.await.unwrap();
    let parts = [(1, one), (2, two)];
    let refused = backend.complete_upload("k", &id, &parts).await;
    let invalid = matches!(refused, Err(Error::InvalidPart(_)));
    assert!(invalid, "{refused:?}");
    assert_eq!(s3.incomplete_uploads(), 1);
}

/// Asserts that a part whose body gives more or fewer bytes than the length told is refused as
/// `Other`, and is not stored.
#[tokio::test]
async fn a_part_of_another_length_than_told_is_refused_and_not_stored() {
    let (s3, backend) = backend().await;
    let id = backend.create_upload("k").await.unwrap();
    for len in [1, 5] {
        let body = chunks(&["ab", "c"]);
        let sending = backend.upload_part_with_len("k", &id, 1, Some(len), body);
        assert_other(sending.await);
    }
    assert_eq!(s3.stats().parts, 0);
}

/// Asserts that the S3 backend gives its tags without quotes, as the other backends do: the
/// tag of a head and that of a put with a tag are the MD5 of the bytes, and a put with a tag
/// takes it with quotes or without; the tag of a completion is the one a head gives the object
/// afterwards, ends with the number of parts and is not the tag of a put of the same bytes.
#[tokio::test]
async fn tags_are_given_without_quotes_and_taken_with_or_without() {
    let (s3, backend) = backend().await;
    s3.set_min_part_bytes(0);
    backend.put("k", body("one")).await.unwrap();
    let tag = backend.head("k").await.unwrap().tag;
    assert_eq!((tag.len(), tag.contains('"')), (32, false));
    let quoted = format!("\"{tag}\"");
    let swap = backend.put_if_match("k", body("two"), &quoted);
    let second = swap.await.unwrap();
    assert_eq!((second.len(), second.contains('"')), (32, false));
    assert_eq!(backend.head("k").await.unwrap().tag, second);
    let swap = backend.put_if_match("k", body("three"), &second);
    let third = swap.await.unwrap();
    assert_eq!(backend.head("k").await.unwrap().tag, third);

    let id = backend.create_upload("m").await.unwrap();
    let uploading = backend.upload_part("m", &id, 1, chunks(&["ab"]));
    let one = uploading.await.unwrap();
    let uploading = backend.upload_part("m", &id, 2, chunks(&["cd"]));
    let two = uploading.await.unwrap();
    let parts = [(1, one), (2, two)];
    let made = backend.complete_upload("m", &id, &parts).await.unwrap();
    assert_eq!(backend.head("m").await.unwrap().tag, made);
    assert!(!made.contains('"'), "{made}");
    assert!(made.ends_with("-2"), "{made}");
    backend.put("plain", body("abcd")).await.unwrap();
    assert_ne!(backend.head("plain").await.unwrap().tag, made);
}

/// Asserts that a streamed put to S3 creates the object from its chunks, with the size and the
/// tag a put of the same bytes gives, whether the length is told or not.
#[tokio::test]
async fn a_streamed_put_creates_the_object_from_its_chunks() {
    let (_s3, backend) = backend().await;
    conformance::a_streamed_put_creates_the_object_from_its_chunks(&backend).await;
}

/// Asserts that a streamed put to S3 whose body fails leaves nothing and gives the error.
#[tokio::test]
async fn a_streamed_put_that_fails_leaves_nothing() {
    let (_s3, backend) = backend().await;
    conformance::a_streamed_put_that_fails_leaves_nothing(&backend).await;
}

/// Asserts that a streamed put to S3 of an existing key is refused and the first object stays.
#[tokio::test]
async fn a_streamed_put_of_an_existing_key_is_refused() {
    let (_s3, backend) = backend().await;
    conformance::a_streamed_put_of_an_existing_key_is_refused(&backend).await;
}

/// The number of bytes in a chunk of a large body.
const MEBIBYTE: usize = 1 << 20;

/// A body of `count` chunks of a mebibyte, made as they are asked for, that then ends as `end`
/// says.
fn generated(count: usize, end: Option<Error>) -> PartBody<'static> {
    let made = stream::iter(0..count).map(|n| Ok(Bytes::from(vec![n as u8; MEBIBYTE])));
    let last = stream::iter(end.map(Err));
    made.chain(last).boxed()
}

/// Asserts that a body longer than one request carries (17 MiB, over the 16 MiB a request
/// holds) is stored by S3 as a multipart upload that is completed only after the last chunk:
/// a body that fails at its last byte leaves no object and no open upload, a key that exists
/// is refused after the upload was aborted, and a whole body makes one object of its length.
#[tokio::test]
async fn a_large_streamed_put_is_completed_only_after_the_last_chunk() {
    let (s3, backend) = backend().await;
    let len = Some(17 * MEBIBYTE as u64);
    let cut = Error::Other("cut".to_owned());

    let failed = backend
        .put_stream("big", len, generated(17, Some(cut.clone())))
        .await;
    assert_eq!(failed, Err(cut));
    assert_eq!(s3.keys(), Vec::<String>::new());
    assert_eq!(s3.incomplete_uploads(), 0);
    assert_eq!(s3.stats().completed, 0);

    backend
        .put_stream("big", len, generated(17, None))
        .await
        .unwrap();
    assert_eq!(
        backend.head("big").await.unwrap().size,
        17 * MEBIBYTE as u64
    );
    assert_eq!(s3.stats().completed, 1);

    let again = backend.put_stream("big", len, generated(17, None)).await;
    assert_eq!(again, Err(Error::AlreadyExists("big".to_owned())));
    assert_eq!(s3.incomplete_uploads(), 0);
    assert_eq!(s3.stats().completed, 1);
}

/// Asserts that the S3 backend says it needs the length of a streamed body, the in-memory
/// backend does not, and a proxy needs it when either of its tiers does.
#[tokio::test]
async fn a_proxy_needs_the_length_of_a_body_when_either_tier_is_s3() {
    let (_s3, s3) = backend().await;
    assert!(s3.needs_length());
    assert!(!tierstore::MemoryBackend::new().needs_length());
    let s3: Arc<dyn Backend> = Arc::new(s3);
    let memory = || -> Arc<dyn Backend> { Arc::new(tierstore::MemoryBackend::new()) };
    let open = |hot: Arc<dyn Backend>, cold: Arc<dyn Backend>| async move {
        let clock = Arc::new(SystemClock);
        Proxy::open(hot, cold, ProxyConfig::new("p1", 1000, clock))
            .await
            .unwrap()
    };
    assert!(open(memory(), s3.clone()).await.needs_length());
    assert!(open(s3.clone(), memory()).await.needs_length());
    assert!(!open(memory(), memory()).await.needs_length());
}
