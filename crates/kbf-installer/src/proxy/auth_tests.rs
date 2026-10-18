use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use hyper::Request;

use super::auth::{
    Authorize, BearerAuth, Denied, FileTokenSource, PeerKey, Principal, RateLimit, Secret,
    Sha256Hasher, TokenHasher, TokenSource,
};
use super::fakes::{CountingHasher, FakeClock, FakeTokens, TOKEN};
use super::traits::{Clock, SystemClock};

struct Rig {
    auth: BearerAuth,
    hashed: Arc<std::sync::atomic::AtomicUsize>,
    clock: Arc<FakeClock>,
    tokens: Arc<FakeTokens>,
}

struct Shared(Arc<FakeTokens>);

impl TokenSource for Shared {
    fn token(&self) -> Option<Secret> {
        self.0.token()
    }
}

fn rig(rate: RateLimit) -> Rig {
    let hasher = CountingHasher::default();
    let hashed = hasher.0.clone();
    let clock = Arc::new(FakeClock(AtomicU64::new(0)));
    let tokens = Arc::new(FakeTokens(Mutex::new(Some(TOKEN.to_owned()))));
    let auth = BearerAuth::new(
        Box::new(Shared(tokens.clone())),
        Box::new(hasher),
        clock.clone(),
        rate,
    );
    Rig {
        auth,
        hashed,
        clock,
        tokens,
    }
}

fn big() -> RateLimit {
    RateLimit {
        burst: 1000,
        per_second: 1000,
    }
}

fn request(uri: &str, headers: &[&str]) -> Request<Bytes> {
    let mut b = Request::builder().uri(uri);
    for h in headers {
        b = b.header("authorization", *h);
    }
    b.body(Bytes::new()).unwrap()
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

#[test]
fn the_auth_matrix() {
    let r = rig(big());
    let right = bearer(TOKEN);
    let off_by_one = bearer(&format!("{}0", &TOKEN[..TOKEN.len() - 1]));
    let prefix = bearer(&TOKEN[..TOKEN.len() - 1]);
    let longer = bearer(&format!("{TOKEN}0"));
    let cases: Vec<(&str, Vec<&str>, Result<Principal, Denied>)> = vec![
        ("/x", vec![], Err(Denied::MissingCredentials)),
        ("/x", vec!["Basic dXNlcjpwYXNz"], Err(Denied::BadScheme)),
        ("/x", vec!["Bearer"], Err(Denied::BadScheme)),
        ("/x", vec!["Token abc"], Err(Denied::BadScheme)),
        ("/x", vec![TOKEN], Err(Denied::BadScheme)),
        ("/x", vec!["Bearer "], Err(Denied::BadToken)),
        ("/x", vec!["Bearer wrong"], Err(Denied::BadToken)),
        ("/x", vec![&off_by_one], Err(Denied::BadToken)),
        ("/x", vec![&prefix], Err(Denied::BadToken)),
        ("/x", vec![&longer], Err(Denied::BadToken)),
        ("/x", vec![&right], Ok(Principal::Operator)),
        ("/x?verbose=1", vec![&right], Ok(Principal::Operator)),
        ("/x", vec![&right, &right], Err(Denied::Malformed)),
    ];
    for (uri, headers, want) in cases {
        assert_eq!(
            r.auth.authorize(&request(uri, &headers)),
            want,
            "{uri} {headers:?}"
        );
    }
    // The scheme is case-insensitive.
    assert_eq!(
        r.auth
            .authorize(&request("/x", &[&format!("BEARER {TOKEN}")])),
        Ok(Principal::Operator)
    );
}

#[test]
fn a_token_in_the_query_string_is_refused_even_with_the_right_header() {
    let r = rig(big());
    for q in [
        format!("/x?token={TOKEN}"),
        format!("/x?a=1&access_token={TOKEN}"),
        format!("/x?Bearer={TOKEN}"),
        format!("/x?api_key={TOKEN}"),
        "/x?token=".to_owned(),
        "/x?token".to_owned(),
    ] {
        assert_eq!(
            r.auth.authorize(&request(&q, &[&bearer(TOKEN)])),
            Err(Denied::TokenInQuery),
            "{q}"
        );
        assert_eq!(
            r.auth.authorize(&request(&q, &[])),
            Err(Denied::TokenInQuery),
            "{q}"
        );
    }
}

#[test]
fn a_server_with_no_token_refuses_everyone() {
    let r = rig(big());
    *r.tokens.0.lock().unwrap() = None;
    assert_eq!(
        r.auth.authorize(&request("/x", &[&bearer(TOKEN)])),
        Err(Denied::TokenUnavailable)
    );
    assert_eq!(
        r.auth.authorize(&request("/x", &[&bearer("")])),
        Err(Denied::BadToken),
        "an empty token never reaches the source"
    );
}

#[test]
fn an_oversized_token_is_refused_without_hashing_it() {
    let r = rig(big());
    let huge = bearer(&"a".repeat(513));
    assert_eq!(
        r.auth.authorize(&request("/x", &[&huge])),
        Err(Denied::BadToken)
    );
    assert_eq!(r.hashed.load(Ordering::SeqCst), 0);
}

#[test]
fn the_rate_limit_applies_before_any_hashing() {
    let r = rig(RateLimit {
        burst: 3,
        per_second: 1,
    });
    for _ in 0..3 {
        assert_eq!(
            r.auth.authorize(&request("/x", &[&bearer("wrong")])),
            Err(Denied::BadToken)
        );
    }
    let hashed = r.hashed.load(Ordering::SeqCst);
    assert_eq!(hashed, 6, "two digests per checked token");
    // Limited: even the right token, even junk, costs no hash.
    for header in [bearer(TOKEN), bearer("wrong"), "Basic x".to_owned()] {
        match r.auth.authorize(&request("/x", &[&header])) {
            Err(Denied::RateLimited { retry_after_s }) => assert_eq!(retry_after_s, 1),
            other => panic!("expected a rate limit, got {other:?}"),
        }
    }
    assert_eq!(
        r.hashed.load(Ordering::SeqCst),
        hashed,
        "no hashing once limited"
    );
    // A second later one more call is allowed.
    r.clock.0.store(1000, Ordering::SeqCst);
    assert_eq!(
        r.auth.authorize(&request("/x", &[&bearer(TOKEN)])),
        Ok(Principal::Operator)
    );
    assert!(matches!(
        r.auth.authorize(&request("/x", &[&bearer(TOKEN)])),
        Err(Denied::RateLimited { .. })
    ));
}

#[test]
fn clients_are_limited_one_by_one() {
    let r = rig(RateLimit {
        burst: 1,
        per_second: 1,
    });
    let from = |peer: &str| {
        let mut req = request("/x", &[&bearer(TOKEN)]);
        req.extensions_mut().insert(PeerKey(peer.to_owned()));
        req
    };
    assert_eq!(r.auth.authorize(&from("10.0.0.1")), Ok(Principal::Operator));
    assert!(matches!(
        r.auth.authorize(&from("10.0.0.1")),
        Err(Denied::RateLimited { .. })
    ));
    assert_eq!(r.auth.authorize(&from("10.0.0.2")), Ok(Principal::Operator));
}

#[test]
fn a_flood_of_peers_cannot_grow_the_table_and_shares_one_bucket() {
    let r = rig(RateLimit {
        burst: 1,
        per_second: 1,
    });
    let from = |peer: String| {
        let mut req = request("/x", &[&bearer(TOKEN)]);
        req.extensions_mut().insert(PeerKey(peer));
        req
    };
    for i in 0..1024 {
        assert_eq!(
            r.auth.authorize(&from(format!("p{i}"))),
            Ok(Principal::Operator)
        );
    }
    // The table is full: new peers share the bucket of "unknown".
    assert_eq!(
        r.auth.authorize(&from("late-1".to_owned())),
        Ok(Principal::Operator)
    );
    assert!(matches!(
        r.auth.authorize(&from("late-2".to_owned())),
        Err(Denied::RateLimited { .. })
    ));
}

#[test]
fn denials_have_stable_statuses_and_one_code_for_the_three_401s() {
    use hyper::StatusCode as S;
    let codes: Vec<_> = [
        Denied::MissingCredentials,
        Denied::BadScheme,
        Denied::BadToken,
    ]
    .into_iter()
    .map(|d| (d.status(), d.code()))
    .collect();
    assert_eq!(codes, vec![(S::UNAUTHORIZED, "unauthorized"); 3]);
    assert_eq!(
        Denied::RateLimited { retry_after_s: 1 }.status(),
        S::TOO_MANY_REQUESTS
    );
    assert_eq!(Denied::TokenInQuery.status(), S::BAD_REQUEST);
    assert_eq!(Denied::Malformed.status(), S::BAD_REQUEST);
    assert_eq!(Denied::TokenUnavailable.status(), S::SERVICE_UNAVAILABLE);
    assert_eq!(Denied::TokenInQuery.code(), "token_in_query");
    assert_eq!(Denied::Malformed.code(), "malformed_credentials");
    assert_eq!(Denied::TokenUnavailable.code(), "auth_unavailable");
    assert_eq!(
        Denied::RateLimited { retry_after_s: 1 }.code(),
        "rate_limited"
    );
}

#[test]
fn the_token_file_is_trimmed_reloaded_and_must_be_long_enough() {
    let dir = kbf_host::testkit::scratch("u8a-tokenfile");
    let path = dir.join("operator.token");
    let src = FileTokenSource::new(&path);
    assert!(src.token().is_none(), "no file, no token");
    std::fs::write(&path, format!("{TOKEN}\n")).unwrap();
    assert_eq!(src.token(), Some(Secret::new(TOKEN)));
    std::fs::write(&path, "short\n").unwrap();
    assert!(src.token().is_none(), "a weak token is not used");
    std::fs::write(&path, "").unwrap();
    assert!(src.token().is_none());
    let rotated = "f".repeat(64);
    std::fs::write(&path, &rotated).unwrap();
    assert_eq!(src.token(), Some(Secret::new(rotated)));
}

#[test]
fn the_digest_is_sha256() {
    assert_eq!(
        hex::encode(Sha256Hasher.digest(b"abc")),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn the_system_clock_moves_forward() {
    let clock = SystemClock::default();
    let a = clock.mono_ms();
    assert!(clock.mono_ms() >= a);
    assert!(clock.wall_s() > 1_700_000_000);
}
