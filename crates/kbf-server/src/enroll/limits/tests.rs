//! The guard of the enroll listener: the rate limit per source, the calls in flight and
//! the mapping of an oversized body. Each test names the defect it catches.

use std::net::Ipv6Addr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tonic::codegen::Service as _;

use super::*;

fn ip(text: &str) -> IpAddr {
    text.parse().expect("address")
}

fn limiter(per_minute: u32, burst: u32) -> RateLimiter {
    RateLimiter::new(&Limits {
        per_minute,
        burst,
        in_flight: 8,
    })
}

/// A source gets its burst at once, then one call per refill period, and no more than the
/// burst ever accumulates.
///
/// Catches: a limiter that never refuses, one that never refills (a fleet restart after an
/// outage would lock every node out for good), and one whose bucket grows without bound
/// while idle (a quiet hour would buy a flood).
#[test]
fn a_source_gets_its_burst_then_the_refill_rate_and_no_more() {
    let limiter = limiter(60, 3);
    let t0 = Instant::now();
    let a = ip("192.0.2.1");
    assert!([0, 0, 0].iter().all(|_| limiter.admit(a, t0)));
    assert!(!limiter.admit(a, t0), "the fourth call at once");
    assert!(!limiter.admit(a, t0 + Duration::from_millis(900)));
    assert!(
        limiter.admit(a, t0 + Duration::from_secs(1)),
        "one token per second"
    );
    assert!(!limiter.admit(a, t0 + Duration::from_secs(1)));
    let later = t0 + Duration::from_secs(3600);
    assert!([0, 0, 0].iter().all(|_| limiter.admit(a, later)));
    assert!(
        !limiter.admit(a, later),
        "an idle hour buys the burst and no more"
    );
}

/// Each source has its own bucket; an IPv6 source is its /64, an IPv4-mapped IPv6 address
/// is the IPv4 address.
///
/// Catches: one noisy node starving the rest of the fleet, an attacker rotating through
/// the addresses of its /64 (free to a v6 host) to dodge the limit, and a mapped address
/// getting a second bucket beside the IPv4 one.
#[test]
fn sources_are_separate_and_a_v6_prefix_is_one_source() {
    let limiter = limiter(1, 1);
    let t0 = Instant::now();
    assert!(limiter.admit(ip("192.0.2.1"), t0));
    assert!(!limiter.admit(ip("192.0.2.1"), t0));
    assert!(limiter.admit(ip("192.0.2.2"), t0), "another source");

    assert!(limiter.admit(ip("2001:db8:1:2::1"), t0));
    assert!(!limiter.admit(ip("2001:db8:1:2:ffff::7"), t0), "same /64");
    assert!(limiter.admit(ip("2001:db8:1:3::1"), t0), "another /64");

    let mapped = IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0xc000, 0x0203));
    assert!(limiter.admit(ip("192.0.2.3"), t0));
    assert!(!limiter.admit(mapped, t0), "192.0.2.3 mapped into IPv6");
    assert_eq!(limiter.tracked(), 5);
}

/// The table is bounded: when every slot is a source that is spending, a new source is
/// refused; sources that have refilled are forgotten to make room.
///
/// Catches: memory growing with the number of addresses that ever called (a spoofed or
/// v4-wide scan), and the opposite: a full table of idle sources refusing real nodes.
#[test]
fn the_table_is_bounded_and_forgets_sources_that_are_idle() {
    let limiter = limiter(60, 2);
    let t0 = Instant::now();
    for n in 0..u32::try_from(MAX_SOURCES).expect("count") {
        assert!(limiter.admit(IpAddr::V4(Ipv4Addr::from(0x0a00_0000 + n)), t0));
    }
    assert_eq!(limiter.tracked(), MAX_SOURCES);
    assert!(
        !limiter.admit(ip("192.0.2.9"), t0),
        "every slot is spending"
    );
    assert_eq!(limiter.tracked(), MAX_SOURCES);
    let later = t0 + Duration::from_secs(10);
    assert!(
        limiter.admit(ip("192.0.2.9"), later),
        "the idle ones were forgotten"
    );
    assert_eq!(limiter.tracked(), 1);
}

/// An inner service that answers when told to.
#[derive(Clone)]
struct Inner {
    gate: Option<Arc<Notify>>,
    status: &'static str,
}

impl tonic::codegen::Service<http::Request<()>> for Inner {
    type Response = http::Response<String>;
    type Error = std::convert::Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: http::Request<()>) -> Self::Future {
        let (gate, status) = (self.gate.clone(), self.status);
        async move {
            if let Some(gate) = gate {
                gate.notified().await;
            }
            Ok(http::Response::builder()
                .header("grpc-status", status)
                .body("answer".to_owned())
                .expect("response"))
        }
        .boxed()
    }
}

fn guard(limits: Limits, inner: Inner) -> Guard<Inner> {
    GuardLayer::new(&limits).layer(inner)
}

fn grpc_status<B>(response: &http::Response<B>) -> (String, String) {
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            // `grpc-message` is percent-encoded on the wire.
            .map(|v| v.to_str().expect("text").replace("%20", " "))
            .unwrap_or_default()
    };
    (header("grpc-status"), header("grpc-message"))
}

fn rate_limited() -> (String, String) {
    (
        RefusalReason::RateLimited.code().number().to_string(),
        RefusalReason::RateLimited.message().to_owned(),
    )
}

/// A call over the burst is answered `RateLimited`, with exactly that code and message,
/// and never reaches the service.
///
/// Catches: a limiter that is built but not wired into the server, and a refusal in some
/// other form than the closed set (a bare HTTP 429 that the node client cannot read).
#[tokio::test]
async fn a_call_over_the_burst_is_rate_limited_and_never_reaches_the_service() {
    let mut guard = guard(
        Limits {
            per_minute: 1,
            burst: 2,
            in_flight: 8,
        },
        Inner {
            gate: None,
            status: "0",
        },
    );
    for _ in 0..2 {
        let ok = guard.call(http::Request::new(())).await.expect("answer");
        assert_eq!(ok.body(), "answer");
    }
    let refused = guard.call(http::Request::new(())).await.expect("answer");
    assert_eq!(grpc_status(&refused), rate_limited());
    assert_eq!(refused.body(), "", "not the service's body");
}

/// At most `in_flight` calls run at once; a slot is given back when its call ends, also
/// when the caller goes away mid-call.
///
/// Catches: the cap leaking a slot per cancelled call (the listener would stop answering
/// after a few hundred dropped connections), and a cap that is not enforced.
#[tokio::test]
async fn calls_in_flight_are_capped_and_the_slot_comes_back() {
    let gate = Arc::new(Notify::new());
    let mut guard = guard(
        Limits {
            per_minute: 600,
            burst: 100,
            in_flight: 1,
        },
        Inner {
            gate: Some(Arc::clone(&gate)),
            status: "0",
        },
    );
    let first = tokio::spawn(guard.call(http::Request::new(())));
    tokio::task::yield_now().await;
    let second = guard.call(http::Request::new(())).await.expect("answer");
    assert_eq!(
        grpc_status(&second),
        rate_limited(),
        "one in flight already"
    );

    gate.notify_one();
    let done = first.await.expect("task").expect("answer");
    assert_eq!(done.body(), "answer");

    // A call that is dropped before it ends gives its slot back.
    let abandoned = tokio::spawn(guard.call(http::Request::new(())));
    tokio::task::yield_now().await;
    abandoned.abort();
    let _ = abandoned.await;
    gate.notify_one();
    let again = guard.call(http::Request::new(())).await.expect("answer");
    assert_eq!(again.body(), "answer", "the slot was given back");
}

/// A body over the decoding limit (tonic's `OUT_OF_RANGE`) is told as `MalformedRequest`;
/// any other answer passes through untouched.
///
/// Catches: a node told a status outside the closed set for an oversized request, and a
/// guard that rewrites answers it should not.
#[tokio::test]
async fn an_oversized_body_is_malformed_request_and_other_answers_pass() {
    let big = Inner {
        gate: None,
        status: "11",
    };
    let mut guard = guard(Limits::DEFAULT, big);
    let refused = guard.call(http::Request::new(())).await.expect("answer");
    assert_eq!(
        grpc_status(&refused),
        (
            RefusalReason::MalformedRequest.code().number().to_string(),
            RefusalReason::MalformedRequest.message().to_owned()
        )
    );
    for status in ["0", "5", "16"] {
        let mut guard = guard_with(status);
        let answer = guard.call(http::Request::new(())).await.expect("answer");
        assert_eq!(grpc_status(&answer).0, status);
        assert_eq!(answer.body(), "answer");
    }
}

fn guard_with(status: &'static str) -> Guard<Inner> {
    guard(Limits::DEFAULT, Inner { gate: None, status })
}

/// The cap is 16 KiB and the listener decodes under it.
///
/// Catches: the constant drifting from the documented one.
#[test]
fn the_request_cap_is_sixteen_kib() {
    assert_eq!(MAX_REQUEST_BYTES, 16 * 1024);
    const { assert!(MAX_REQUEST_BYTES > 2 * kbf_ca::MAX_CSR_LEN + 1024) };
}
