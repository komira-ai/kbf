//! [`RetryPolicy`] and [`Cause`]: the numbers of the retry of a call to the pack store, how its
//! waits grow and are jittered, and which answers of the store are retried and under what cause.

use std::sync::Arc;

use kbf_objstore::{ByteRange, ObjectKey};

use super::*;

const fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

fn service(status: u16, code: &str) -> ObjectStoreError {
    ObjectStoreError::Service {
        status,
        code: code.to_owned(),
        message: "injected".to_owned(),
    }
}

fn key() -> ObjectKey {
    ObjectKey::new("cas/test/a").expect("a key")
}

fn policy(jitter_percent: u8) -> RetryPolicy {
    RetryPolicy {
        jitter_percent,
        ..RetryPolicy::DEFAULT
    }
}

/// Catches defaults that drift from the numbers the module documents: 0.5 s first, doubling, 8 s
/// longest, 50 % jitter, a budget of 120 s; and `with_budget` changing anything but the budget.
#[test]
fn the_default_policy_has_the_documented_numbers() {
    let default = RetryPolicy::default();
    assert_eq!(default, RetryPolicy::DEFAULT);
    assert_eq!(default.first, Duration::from_millis(500));
    assert_eq!(default.growth, 2);
    assert_eq!(default.longest, secs(8));
    assert_eq!(default.jitter_percent, 50);
    assert_eq!(default.budget, secs(120));

    let shorter = default.with_budget(secs(7));
    assert_eq!(shorter.budget, secs(7));
    assert_eq!(
        RetryPolicy {
            budget: default.budget,
            ..shorter
        },
        default
    );
}

/// Catches waits that do not double or that pass the longest: from 0.5 s the nominal waits are
/// 1, 2, 4 and 8 s, and stay at 8 s after.
#[test]
fn a_wait_doubles_and_stops_at_the_longest() {
    let policy = RetryPolicy::DEFAULT;
    let mut wait = policy.first;
    let mut seen = Vec::new();
    for _ in 0..7 {
        wait = policy.grown(wait);
        seen.push(wait);
    }
    let want = [1.0, 2.0, 4.0, 8.0, 8.0, 8.0, 8.0].map(Duration::from_secs_f64);
    assert_eq!(seen, want);
}

/// Catches a growth of 0 that collapses the wait to zero (a retry in a hot loop): it is taken as
/// 1, so the wait stays as it was, and a growth of 1 keeps every wait the same; a huge wait
/// saturates at the longest.
#[test]
fn a_growth_of_zero_is_taken_as_one() {
    let zero = RetryPolicy {
        growth: 0,
        ..RetryPolicy::DEFAULT
    };
    let one = RetryPolicy {
        growth: 1,
        ..RetryPolicy::DEFAULT
    };
    assert_eq!(zero.grown(secs(3)), secs(3));
    assert_eq!(one.grown(secs(3)), secs(3));
    assert_eq!(zero.grown(secs(30)), secs(8));
    let big = RetryPolicy {
        growth: u32::MAX,
        ..RetryPolicy::DEFAULT
    };
    assert_eq!(big.grown(Duration::MAX), secs(8));
}

/// Catches jitter at 0 % that moves a wait: every draw leaves the nominal wait exact.
#[test]
fn zero_percent_jitter_is_exact() {
    let policy = policy(0);
    for z in [0, 1, 4_000_000_000, u64::MAX] {
        assert_eq!(policy.jittered(secs(8), z), secs(8), "draw {z}");
    }
}

/// Catches 50 % jitter outside the second half of the wait, or one that cannot reach either end:
/// of 8 s, draw 0 takes nothing off (8 s), draw 4e9 ns takes off half (4 s), the next draw wraps
/// to 8 s again, and every draw lies in [4 s, 8 s].
#[test]
fn fifty_percent_jitter_lies_in_the_second_half_and_reaches_both_ends() {
    let policy = policy(50);
    let nominal = secs(8);
    assert_eq!(policy.jittered(nominal, 0), nominal);
    assert_eq!(policy.jittered(nominal, 4_000_000_000), secs(4));
    assert_eq!(policy.jittered(nominal, 4_000_000_001), nominal);
    for z in (0..64u64).map(|n| n.wrapping_mul(0x9E37_79B9_7F4A_7C15)) {
        let wait = policy.jittered(nominal, z);
        assert!(wait >= secs(4) && wait <= nominal, "draw {z}: {wait:?}");
    }
}

/// Catches 100 % jitter that cannot reach a wait of zero, or one that goes past it: the draw
/// that takes the whole wait off gives zero, and no draw gives more than the nominal wait.
#[test]
fn a_hundred_percent_jitter_can_reach_zero() {
    let policy = policy(100);
    assert_eq!(policy.jittered(secs(2), 2_000_000_000), Duration::ZERO);
    assert_eq!(policy.jittered(secs(2), 0), secs(2));
    assert!(policy.jittered(secs(2), u64::MAX) <= secs(2));
}

/// Catches a percent above 100 that takes off more than the whole wait (a wait below zero, which
/// saturates wrongly) or panics: it is clamped to 100, so it behaves as 100 for every draw, and a
/// zero wait stays zero.
#[test]
fn a_percent_over_a_hundred_is_taken_as_a_hundred() {
    let over = policy(250);
    let whole = policy(100);
    for z in [0, 1, 999_999_999, 2_000_000_000, u64::MAX] {
        let (got, want) = (over.jittered(secs(2), z), whole.jittered(secs(2), z));
        assert_eq!(got, want, "draw {z}");
    }
    assert_eq!(over.jittered(Duration::ZERO, u64::MAX), Duration::ZERO);
}

/// Catches a retried answer taken as final, or the wrong cause under it: a refused or reset
/// connection is `connection`, a 503 `SlowDown` is `slow_down`, any other 503 is `unavailable`,
/// a 500, 502 and 504 are `server_error`, and a 400 `RequestTimeout` is `request_timeout`.
#[test]
fn every_retried_answer_has_its_cause() {
    let reset = ObjectStoreError::Transport(Arc::new(std::io::Error::other("reset")));
    let cases = [
        (reset, Cause::Connection),
        (service(503, "SlowDown"), Cause::SlowDown),
        (service(503, "ServiceUnavailable"), Cause::Unavailable),
        (service(503, ""), Cause::Unavailable),
        (service(500, "InternalError"), Cause::ServerError),
        (service(502, "BadGateway"), Cause::ServerError),
        (service(504, "GatewayTimeout"), Cause::ServerError),
        (service(400, "RequestTimeout"), Cause::RequestTimeout),
    ];
    for (error, cause) in cases {
        assert_eq!(Cause::of(&error), Some(cause), "{error}");
    }
}

/// Catches an answer retried that the store gives for good, which would repeat a refusal or hide
/// a missing object: a 403, a 404 status, a 400 of another code, a 501, a missing object, an
/// existing one, a bad range, a locked object, an unsupported request, a broken protocol and a
/// failed body are each no cause.
#[test]
fn answers_the_store_gives_for_good_have_no_cause() {
    let range = ByteRange::new(0, 1).expect("a range");
    let answers = [
        service(403, "AccessDenied"),
        service(404, "NoSuchKey"),
        service(404, "SlowDown"),
        service(400, "InvalidArgument"),
        service(501, "NotImplemented"),
        service(200, "RequestTimeout"),
        ObjectStoreError::NotFound(key()),
        ObjectStoreError::AlreadyExists(key()),
        ObjectStoreError::InvalidRange { key: key(), range },
        ObjectStoreError::Locked(key()),
        ObjectStoreError::Unsupported("retention"),
        ObjectStoreError::Protocol("whole object".to_owned()),
        ObjectStoreError::Body("short".to_owned()),
    ];
    for error in answers {
        assert_eq!(Cause::of(&error), None, "{error}");
    }
}

/// Catches labels that collide (two causes counted as one series) or a list that misses a cause:
/// the five labels are distinct, in the order of `ALL`, and as the metric documents them.
#[test]
fn the_labels_are_distinct() {
    let labels = Cause::ALL.map(Cause::label);
    assert_eq!(
        labels,
        [
            "connection",
            "slow_down",
            "unavailable",
            "server_error",
            "request_timeout"
        ]
    );
    for (n, cause) in Cause::ALL.into_iter().enumerate() {
        assert_eq!(cause as usize, n, "{cause:?} is out of order in ALL");
    }
}
