//! The deadline of an upload: the `grpc-timeout` values the gRPC specification allows and no
//! other, the margin, and the call ended DEADLINE_EXCEEDED when its deadline passes. The clock is
//! the runtime's, paused, so a deadline passes when nothing else can run, not by waiting.

use std::future::pending;

use tonic::Code;
use tonic::codegen::http::HeaderValue;

use super::*;

/// Catches a unit read as another, or a value outside the specification taken: each of the six
/// units, at one and at eight digits, and nothing else.
#[test]
fn a_timeout_is_one_to_eight_digits_and_a_unit() {
    let cases: [(&str, Duration); 8] = [
        ("1H", Duration::from_secs(3600)),
        ("2M", Duration::from_secs(120)),
        ("30S", Duration::from_secs(30)),
        ("250m", Duration::from_millis(250)),
        ("7u", Duration::from_micros(7)),
        ("9n", Duration::from_nanos(9)),
        ("99999999H", Duration::from_secs(99_999_999 * 3600)),
        ("00000005S", Duration::from_secs(5)),
    ];
    for (value, want) in cases {
        assert_eq!(parse(value.as_bytes()), Some(want), "{value}");
    }
    for bad in [
        "",
        "S",
        "5",
        "123456789S",
        "5X",
        "5s",
        "-5S",
        "5 S",
        "5.0S",
        "٥S",
    ] {
        assert_eq!(parse(bad.as_bytes()), None, "{bad:?}");
    }
}

/// Catches a margin that eats a short deadline or leaves a long one too close to the transport's
/// cancel: a tenth of the timeout, never more than a second.
#[test]
fn the_margin_is_a_tenth_and_at_most_a_second() {
    assert_eq!(margin(Duration::ZERO), Duration::ZERO);
    assert_eq!(
        margin(Duration::from_millis(500)),
        Duration::from_millis(50)
    );
    assert_eq!(margin(Duration::from_secs(10)), MOST_MARGIN);
    assert_eq!(margin(Duration::from_secs(60)), MOST_MARGIN);
}

/// Catches a deadline taken from the wrong header, or counted without its margin: a request
/// with `grpc-timeout: 10S` ends its work nine seconds from now, from its headers and from its
/// metadata alike; one without the header, or with a value outside the specification, has none.
#[tokio::test(start_paused = true)]
async fn the_deadline_is_the_timeout_less_the_margin() {
    let now = Instant::now();
    let mut headers = HeaderMap::new();
    assert_eq!(of_headers(&headers), None);
    headers.insert(GRPC_TIMEOUT, HeaderValue::from_static("10S"));
    assert_eq!(of_headers(&headers), Some(now + Duration::from_secs(9)));
    let metadata = MetadataMap::from_headers(headers.clone());
    assert_eq!(of_metadata(&metadata), Some(now + Duration::from_secs(9)));
    assert_eq!(of_metadata(&MetadataMap::new()), None);
    headers.insert(GRPC_TIMEOUT, HeaderValue::from_static("ten seconds"));
    assert_eq!(of_headers(&headers), None);
    assert_eq!(of_metadata(&MetadataMap::from_headers(headers)), None);
}

/// Catches work that waits past its deadline, or an end that is not DEADLINE_EXCEEDED: work that
/// never ends, given a deadline, is ended DEADLINE_EXCEEDED.
#[tokio::test(start_paused = true)]
async fn work_past_its_deadline_is_deadline_exceeded() {
    let at = Instant::now() + Duration::from_secs(9);

    let ended = within(Some(at), pending::<Result<(), Status>>()).await;

    let status = ended.expect_err("ended");
    assert_eq!(status.code(), Code::DeadlineExceeded);
    assert_eq!(
        status.message(),
        "the call's deadline (grpc-timeout) passed before the store took its blobs"
    );
}

/// Catches a deadline that changes an answer given in time: work that ends before its deadline,
/// or with none, is answered as it ended, its value or its own error.
#[tokio::test(start_paused = true)]
async fn work_in_time_is_its_own_answer() {
    let at = Some(Instant::now() + Duration::from_secs(9));
    assert_eq!(within(at, async { Ok(7) }).await.expect("in time"), 7);
    assert_eq!(within(None, async { Ok(8) }).await.expect("no deadline"), 8);
    let refused = within(at, async { Err::<(), _>(Status::invalid_argument("no")) }).await;
    assert_eq!(refused.expect_err("its own").code(), Code::InvalidArgument);
}
