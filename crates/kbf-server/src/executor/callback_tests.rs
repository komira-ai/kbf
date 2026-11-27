use std::pin::pin;
use std::task::{Context, Waker};

use axum::body::Body;
use axum::http::{HeaderValue, Method, Request};
use kbf_front::Finished;
use tracing::Level;

use super::*;
use crate::executor::tests::{Rig, settle};
use crate::test_log::capture_at;

/// The fixed clock of the tests that do not run a job, in seconds since the epoch.
const NOW: u64 = 1_800_000_000;

fn fixed_now() -> u64 {
    NOW
}

fn fill_5a(bytes: &mut [u8]) -> bool {
    bytes.fill(0x5a);
    true
}

fn fill_fails(_: &mut [u8]) -> bool {
    false
}

/// Fills the key and fails for the reference.
fn fill_reference_fails(bytes: &mut [u8]) -> bool {
    bytes.len() == KEY_BYTES
}

fn origin() -> CallbackOrigin {
    "https://kbf.example".parse().expect("an origin")
}

/// The `webhook-signature` of the Standard Webhooks rule under `key` (raw bytes): what the job
/// service's signer writes.
fn sign(key: &[u8], id: &str, timestamp: &str, body: &[u8]) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key);
    let mut signed = format!("{id}.{timestamp}.").into_bytes();
    signed.extend_from_slice(body);
    format!(
        "v1,{}",
        STANDARD.encode(ring::hmac::sign(&key, &signed).as_ref())
    )
}

fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        headers.append(*name, HeaderValue::from_str(value).expect("a header value"));
    }
    headers
}

/// The headers of a call `id` at `timestamp`, signed under `key` over `body`.
fn call(key: &[u8], id: &str, timestamp: &str, body: &[u8]) -> HeaderMap {
    let signature = sign(key, id, timestamp, body);
    headers(&[
        ("webhook-id", id),
        ("webhook-timestamp", timestamp),
        ("webhook-signature", signature.as_str()),
    ])
}

/// Whether `armed`'s wait completes now, without blocking.
fn woke(armed: &Armed) -> bool {
    let mut wait = pin!(woken(Some(armed)));
    let mut context = Context::from_waker(Waker::noop());
    wait.as_mut().poll(&mut context).is_ready()
}

/// A set on the fixed clock holding `reference` under `key`.
fn holding(reference: &str, key: &[u8]) -> (Callbacks, Armed) {
    let callbacks = Callbacks::with_sources(fill_5a, fixed_now);
    let armed = callbacks.hold(reference.to_owned(), Zeroizing::new(key.to_vec()));
    (callbacks, armed)
}

/// Asserts the forms `--executor-callback-origin` takes and refuses, with the exact text of
/// each refusal: an https host, with a port of 1 to 65535 and IPv6 literals, is taken (the
/// scheme written in lower case); another scheme, nothing after the scheme, a path, a query, a
/// fragment, a user, a port of 0, 65536 or letters, and a host longer than a name may be are
/// refused. Catches an origin the job service would refuse at every submit (every job would
/// then fail), and a path that doubles the `/js-done` the executor adds.
#[test]
fn the_callback_origin_is_an_https_host_and_port_only() {
    for (given, kept) in [
        ("https://kbf.example", "https://kbf.example"),
        ("HTTPS://kbf.example:8443", "https://kbf.example:8443"),
        ("https://192.0.2.7:1", "https://192.0.2.7:1"),
        ("https://[fd00::7]", "https://[fd00::7]"),
        ("https://[fd00::7]:65535", "https://[fd00::7]:65535"),
    ] {
        let origin: CallbackOrigin = given.parse().expect(given);
        assert_eq!(origin.to_string(), kept);
    }
    let not_https = "the callback origin must start with https://";
    let not_origin = "the callback origin is https://HOST or https://HOST:PORT (a port of 1 to 65535), with no user, path, query or fragment";
    let long_host = format!("https://{}", "a".repeat(254));
    for (given, text) in [
        ("http://kbf.example", not_https),
        ("kbf.example", not_https),
        ("https:/", not_https),
        ("https://", not_origin),
        ("https://kbf.example/", not_origin),
        ("https://kbf.example/js-done", not_origin),
        ("https://kbf.example?x", not_origin),
        ("https://kbf.example#x", not_origin),
        ("https://u@kbf.example", not_origin),
        ("https://kbf.example:0", not_origin),
        ("https://kbf.example:65536", not_origin),
        ("https://kbf.example:443x", not_origin),
        ("https://kbf.example:", not_origin),
        ("https://kbf.example:123456", not_origin),
        (long_host.as_str(), not_origin),
    ] {
        let refused = given.parse::<CallbackOrigin>().expect_err(given);
        assert_eq!(refused.to_string(), text, "{given}");
    }
}

/// Asserts what arming a job does: with no origin, nothing (the spec keeps no callback and no
/// run waits); with one, the spec's callback is `<origin>/js-done/<32 hex digits>` with the 32
/// minted bytes as its key and a deadline of [`DEADLINE_SECONDS`], one run waits, and dropping
/// the hold forgets it. The spec asks for no retry policy either way. Catches a key or a
/// reference not minted for the job, a reference that is not the one held, and a hold that
/// outlives its run.
#[test]
fn arming_mints_the_callback_and_holds_it_until_dropped() {
    let callbacks = Callbacks::with_sources(fill_5a, fixed_now);
    let mut spec = JobSpec::default();
    assert!(callbacks.arm(None, &mut spec).is_none());
    assert_eq!(spec, JobSpec::default());

    let armed = callbacks.arm(Some(&origin()), &mut spec).expect("armed");
    let callback = spec.callback.clone().expect("a callback");
    let reference = "5a".repeat(16);
    assert_eq!(
        callback.url,
        format!("https://kbf.example/js-done/{reference}")
    );
    assert_eq!(callback.signing_key, [0x5a; 32]);
    assert_eq!(callback.deadline_seconds, Some(600));
    assert_eq!(spec.retry, None);
    assert_eq!(armed.reference, reference);
    assert_eq!(callbacks.waiting_count(), 1);
    drop(armed);
    assert_eq!(callbacks.waiting_count(), 0);
    let at = NOW.to_string();
    let late = call(&[0x5a; 32], "msg_1", &at, b"");
    assert_eq!(
        callbacks.called(&reference, &late, b""),
        StatusCode::NOT_FOUND
    );
}

/// Asserts that a random source that fails, for the key or for the reference, arms nothing:
/// the spec keeps no callback, no run waits, and one warning each names the job and not a key. Catches a job submitted with a key of
/// zeros, which anyone could sign with.
#[test]
fn a_failed_random_source_arms_nothing() {
    crate::test_log::install();
    let log = capture_at(Level::WARN);
    let sources: [fn(&mut [u8]) -> bool; 2] = [fill_fails, fill_reference_fails];
    for fill in sources {
        let callbacks = Callbacks::with_sources(fill, fixed_now);
        let mut spec = JobSpec {
            job_id: "job-1".to_owned(),
            ..JobSpec::default()
        };
        assert!(callbacks.arm(Some(&origin()), &mut spec).is_none());
        assert_eq!(spec.callback, None);
        assert_eq!(callbacks.waiting_count(), 0);
    }
    let lines = log.text();
    let lines: Vec<_> = lines
        .lines()
        .filter_map(|line| line.split_once("kbf_server::executor::callback: "))
        .map(|(_, message)| message)
        .collect();
    let line = "the random source failed: the job is watched by polling alone job=job-1";
    assert_eq!(lines, [line, line]);
}

/// Asserts the signature rule against two references: the Standard Webhooks specification's
/// own vector (a 24-byte key, a body) and the one the job service's signer is tested with (the
/// bytes 0 to 31 as the key, an empty body). Each verifies at its own timestamp. Catches a
/// receiver that signs another text than `<id>.<timestamp>.<body>`, that decodes the key, or
/// that reads the signature as anything but standard base64.
#[test]
fn a_call_signed_as_the_references_sign_verifies() {
    let key = STANDARD
        .decode("MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw")
        .expect("base64");
    let body = br#"{"test": 2432232314}"#;
    let spec = headers(&[
        ("webhook-id", "msg_p5jXN8AQM9LWM0D4loKWxJek"),
        ("webhook-timestamp", "1614265330"),
        (
            "webhook-signature",
            "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=",
        ),
    ]);
    let callbacks = Callbacks::with_sources(fill_5a, || 1_614_265_330);
    let held = callbacks.hold("spec".to_owned(), Zeroizing::new(key));
    assert_eq!(
        callbacks.called("spec", &spec, body),
        StatusCode::NO_CONTENT
    );
    assert!(woke(&held));

    let key: Vec<u8> = (0..32).collect();
    let service = headers(&[
        ("webhook-id", "msg_2KWPBgLlAfxdpx2AI54pPJ85f4W"),
        ("webhook-timestamp", "1798761600"),
        (
            "webhook-signature",
            "v1,2YmkLa/YKkNC4uexQssZVwnzHzOjtQlyypa8SU0FrjI=",
        ),
    ]);
    let callbacks = Callbacks::with_sources(fill_5a, || 1_798_761_600);
    let held = callbacks.hold("service".to_owned(), Zeroizing::new(key));
    let answer = callbacks.called("service", &service, b"");
    assert_eq!(answer, StatusCode::NO_CONTENT);
    assert!(woke(&held));
}

/// Asserts every refusal of a call, each answered 401 and waking nothing: each header missing,
/// given twice, empty or not text; a timestamp 301 s either side of the clock or not a number;
/// a signature under another key, over another body, of another version, or not base64. A
/// timestamp exactly 300 s either side verifies, and so does a good signature after a bad one
/// in the same header. Catches a check that is skipped, a tolerance off by one, and a receiver
/// that wakes a run for a call anyone could send.
#[test]
fn a_call_that_does_not_verify_is_refused_and_wakes_nothing() {
    let key = [7_u8; 32];
    let (callbacks, armed) = holding("r", &key);
    let at = NOW.to_string();
    let at = at.as_str();
    let good = sign(&key, "m", at, b"");
    let good = good.as_str();
    let other = sign(&[8; 32], "m", at, b"");
    let other = other.as_str();
    let v2 = good.replacen("v1,", "v2,", 1);
    let mut refused = vec![
        headers(&[("webhook-timestamp", at), ("webhook-signature", good)]),
        headers(&[("webhook-id", "m"), ("webhook-signature", good)]),
        headers(&[("webhook-id", "m"), ("webhook-timestamp", at)]),
        headers(&[
            ("webhook-id", "m"),
            ("webhook-id", "m"),
            ("webhook-timestamp", at),
            ("webhook-signature", good),
        ]),
        headers(&[
            ("webhook-id", ""),
            ("webhook-timestamp", at),
            ("webhook-signature", good),
        ]),
        headers(&[
            ("webhook-id", "m"),
            ("webhook-timestamp", at),
            ("webhook-signature", other),
        ]),
        headers(&[
            ("webhook-id", "m"),
            ("webhook-timestamp", at),
            ("webhook-signature", v2.as_str()),
        ]),
        headers(&[
            ("webhook-id", "m"),
            ("webhook-timestamp", at),
            ("webhook-signature", "v1,not*base64"),
        ]),
        headers(&[
            ("webhook-id", "m"),
            ("webhook-timestamp", "soon"),
            ("webhook-signature", good),
        ]),
    ];
    for stale in [NOW - 301, NOW + 301] {
        let stale = stale.to_string();
        refused.push(call(&key, "m", &stale, b""));
    }
    let mut not_text = call(&key, "m", at, b"");
    let bytes = HeaderValue::from_bytes(b"m\xff").expect("a header value of bytes");
    not_text.insert("webhook-id", bytes);
    refused.push(not_text);
    for (n, call) in refused.iter().enumerate() {
        let answer = callbacks.called("r", call, b"");
        assert_eq!(answer, StatusCode::UNAUTHORIZED, "call {n}");
    }
    let over_another_body = call(&key, "m", at, b"");
    let answer = callbacks.called("r", &over_another_body, b"x");
    assert_eq!(answer, StatusCode::UNAUTHORIZED);
    assert!(!woke(&armed), "no refused call wakes the run");

    for edge in [NOW - 300, NOW + 300] {
        let edge = edge.to_string();
        let answer = callbacks.called("r", &call(&key, &edge, &edge, b""), b"");
        assert_eq!(answer, StatusCode::NO_CONTENT, "at {edge}");
    }
    let both = format!("{other} {good}");
    let two = headers(&[
        ("webhook-id", "m"),
        ("webhook-timestamp", at),
        ("webhook-signature", both.as_str()),
    ]);
    assert_eq!(callbacks.called("r", &two, b""), StatusCode::NO_CONTENT);
    assert!(woke(&armed));
}

/// Asserts that a call that verifies wakes its run once: the wait completes, a repeat of the
/// same `webhook-id` (the service sends one again after a lost answer) is answered 204 and
/// wakes nothing more, and a wake that came before the run waited is kept for it. A reference
/// held by no run is 404. Catches a wake lost because it came between two waits, and a run
/// woken over and over by the service's retries.
#[test]
fn a_verified_call_wakes_its_run_once() {
    let key = [9_u8; 32];
    let (callbacks, armed) = holding("r", &key);
    let at = NOW.to_string();
    let good = call(&key, "msg_1", &at, b"");
    assert!(!woke(&armed), "no call, no wake");
    assert_eq!(callbacks.called("r", &good, b""), StatusCode::NO_CONTENT);
    assert_eq!(callbacks.called("r", &good, b""), StatusCode::NO_CONTENT);
    assert!(woke(&armed), "the wake is kept until the run waits");
    assert!(!woke(&armed), "the repeat woke nothing more");
    assert_eq!(callbacks.called("other", &good, b""), StatusCode::NOT_FOUND);
    let mut never = pin!(woken(None));
    let mut context = Context::from_waker(Waker::noop());
    assert!(never.as_mut().poll(&mut context).is_pending());
}

/// Asserts the route through the router, as kbf-proxy forwards a call: a signed POST to
/// `/js-done/<reference>` is 204, a GET of it is 405, a body over 4096 bytes is 413, and a
/// path outside the route is 404. Catches the route left off the operator API, another method
/// taken, and a call read whole however large.
#[tokio::test]
async fn the_route_answers_a_post_and_only_a_post() {
    use tower_service::Service as _;
    let key = [3_u8; 32];
    let (callbacks, _armed) = holding("abc", &key);
    let mut router = routes(callbacks);
    let at = NOW.to_string();
    let send = |method: Method, path: &str, body: Vec<u8>| {
        let mut request = Request::builder().method(method).uri(path);
        for (name, value) in &call(&key, "m", &at, &body) {
            request = request.header(name, value);
        }
        request.body(Body::from(body)).expect("a request")
    };
    let answers = [
        (
            send(Method::POST, "/js-done/abc", Vec::new()),
            StatusCode::NO_CONTENT,
        ),
        (
            send(Method::GET, "/js-done/abc", Vec::new()),
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        (
            send(Method::POST, "/js-done/abc", vec![b'x'; MAX_BODY + 1]),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
        (
            send(Method::POST, "/js-done", Vec::new()),
            StatusCode::NOT_FOUND,
        ),
    ];
    for (request, status) in answers {
        let path = request.uri().to_string();
        let answer = router.call(request).await.expect("an answer");
        assert_eq!(answer.status(), status, "{path}");
    }
}

/// Asserts what the set prints: the number of runs waiting and nothing of a reference or a
/// key, by exact equality, and what a job's callback prints (js-proto's own `Debug`): the
/// origin, the key's length and the deadline. Catches a key or a reference in a panic message
/// or a debug line that prints the executor's state or a job's spec.
#[test]
fn the_callbacks_print_no_reference_and_no_key() {
    let callbacks = Callbacks::with_sources(fill_5a, fixed_now);
    let mut spec = JobSpec::default();
    let _armed = callbacks.arm(Some(&origin()), &mut spec).expect("armed");
    assert_eq!(format!("{callbacks:?}"), "Callbacks { waiting: 1, .. }");
    let callback = spec.callback.expect("a callback");
    assert_eq!(
        format!("{callback:?}"),
        "Callback { url_origin: \"https://kbf.example\", signing_key_len: 32, deadline_seconds: Some(600), .. }"
    );
}

/// The rig with the callback origin of [`origin`].
async fn called_back_rig() -> Rig {
    Rig::with(|cfg| cfg.callback_origin = Some(origin())).await
}

/// The reference and the key of `job`'s callback, as the job service holds them.
fn held_callback(rig: &Rig, job: &str) -> (String, Vec<u8>) {
    let callback = rig.fake.submitted(job).and_then(|spec| spec.callback);
    let callback = callback.expect("the job was submitted with a callback");
    let reference = callback.url.strip_prefix("https://kbf.example/js-done/");
    let reference = reference.expect("the URL is under the origin").to_owned();
    (reference, callback.signing_key)
}

/// Asserts the wake end to end: a job submitted with the origin set carries its callback (and
/// no retry policy), and once it has ended no read is made while the clock stands still; a
/// call that verifies then makes one read at once, without the clock moving, and the run is
/// answered from it. The run's place is forgotten once it is over, so the same call is then
/// 404. Catches a call that wakes nothing (the run would wait for its poll), a run that takes
/// the call itself for the outcome, and a reference held past its run.
#[tokio::test(start_paused = true)]
async fn a_verified_call_reads_the_ended_job_back_at_once() {
    let rig = called_back_rig().await;
    let caller = rig.submit(1);
    let job = rig.submitted(&caller, 0).await;
    let spec = rig.fake.submitted(&job).expect("submitted");
    assert_eq!(spec.retry, None);
    assert_eq!(
        spec.callback.as_ref().and_then(|c| c.deadline_seconds),
        Some(600)
    );
    let (reference, key) = held_callback(&rig, &job);
    assert_eq!(reference.len(), 32);
    assert!(reference.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(key.len(), 32);

    rig.host_runs(&job);
    rig.advance(rig.cfg.poll).await;
    assert!(rig.fake.finish(&job, 0));
    settle().await;
    let reads = rig.calls_of("GetJob");
    settle().await;
    assert_eq!(
        rig.calls_of("GetJob"),
        reads,
        "no read while the clock stands"
    );

    let at = system_now().to_string();
    let signed = call(&key, "msg_1", &at, b"");
    let callbacks = rig.exec.callbacks();
    assert_eq!(
        callbacks.called(&reference, &signed, b""),
        StatusCode::NO_CONTENT
    );
    settle().await;
    assert_eq!(rig.calls_of("GetJob"), reads + 1, "one read, at once");
    for _ in 0..8 {
        settle().await;
    }
    assert!(matches!(caller.finished(), Some(Finished::Ran(_))));
    assert_eq!(callbacks.waiting_count(), 0);
    assert_eq!(
        callbacks.called(&reference, &signed, b""),
        StatusCode::NOT_FOUND
    );
}

/// Asserts the fallback: a job whose call never comes, or comes refused (a forged signature),
/// still ends by polling. No read is made for the refused call; the next poll reads the ended
/// job and the run is answered. Catches a run that waits for the call alone, and a refused
/// call that wakes it.
#[tokio::test(start_paused = true)]
async fn a_lost_or_refused_call_still_ends_the_job_by_polling() {
    let rig = called_back_rig().await;
    let caller = rig.submit(2);
    let job = rig.submitted(&caller, 0).await;
    let (reference, _key) = held_callback(&rig, &job);
    rig.host_runs(&job);
    rig.advance(rig.cfg.poll).await;
    assert!(rig.fake.finish(&job, 0));
    settle().await;
    let reads = rig.calls_of("GetJob");
    let at = system_now().to_string();
    let forged = call(&[0; 32], "msg_1", &at, b"");
    let callbacks = rig.exec.callbacks();
    assert_eq!(
        callbacks.called(&reference, &forged, b""),
        StatusCode::UNAUTHORIZED
    );
    settle().await;
    assert_eq!(
        rig.calls_of("GetJob"),
        reads,
        "a refused call reads nothing"
    );
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
    assert!(rig.calls_of("GetJob") > reads, "the poll read the job");
    assert_eq!(callbacks.waiting_count(), 0);
}

/// Asserts that without the origin a job asks for no callback and no retry policy, and no run
/// waits for a call: the job service is asked exactly what it was before callbacks. Catches a
/// callback sent to a service that may refuse it, which would fail every job.
#[tokio::test(start_paused = true)]
async fn without_the_origin_a_job_asks_for_no_callback() {
    let rig = Rig::start().await;
    let caller = rig.submit(3);
    let job = rig.submitted(&caller, 0).await;
    let spec = rig.fake.submitted(&job).expect("submitted");
    assert_eq!((spec.callback, spec.retry), (None, None));
    assert_eq!(rig.exec.callbacks().waiting_count(), 0);
    rig.host_finishes(&job, 0);
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
}

/// Asserts that the key and the reference of a job's callback are in no log line, at every
/// level, over the whole life of the job: its submit, a refused call (a forged signature), a
/// stale call, the call that verifies, and its end. The key is looked for in hex and in
/// base64, the reference and the URL as written; the lines the callback code writes are, by
/// exact equality, its two refusals. Catches a log line or an error text that carries the
/// key, the URL's path or the reference.
#[tokio::test(start_paused = true)]
async fn the_key_and_the_reference_are_in_no_log_line() {
    let log = capture_at(Level::TRACE);
    let rig = called_back_rig().await;
    let caller = rig.submit(4);
    let job = rig.submitted(&caller, 0).await;
    let (reference, key) = held_callback(&rig, &job);
    rig.host_runs(&job);
    rig.advance(rig.cfg.poll).await;
    assert!(rig.fake.finish(&job, 0));
    let now = system_now();
    let callbacks = rig.exec.callbacks();
    let forged = call(&[0; 32], "msg_1", &now.to_string(), b"");
    let stale = call(&key, "msg_1", &(now - 3600).to_string(), b"");
    let good = call(&key, "msg_1", &now.to_string(), b"");
    for (signed, status) in [
        (forged, StatusCode::UNAUTHORIZED),
        (stale, StatusCode::UNAUTHORIZED),
        (good, StatusCode::NO_CONTENT),
    ] {
        assert_eq!(callbacks.called(&reference, &signed, b""), status);
    }
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));

    let text = log.text();
    let url = format!("https://kbf.example/js-done/{reference}");
    // A failure names which form leaked, and prints neither the secret nor the log that holds it.
    let forms = [
        ("the key in hex", hex::encode(&key)),
        ("the key in base64", STANDARD.encode(&key)),
        ("the reference", reference),
        ("the URL", url),
    ];
    for (form, secret) in forms {
        assert!(!text.contains(&secret), "{form} is in a log line");
    }
    let own: Vec<_> = text
        .lines()
        .filter_map(|line| line.split_once("kbf_server::executor::callback: "))
        .map(|(_, message)| message)
        .collect();
    assert_eq!(
        own,
        [
            "a completion call was refused: no signature verifies",
            "a completion call was refused: its timestamp is not within the tolerance",
        ]
    );
}
