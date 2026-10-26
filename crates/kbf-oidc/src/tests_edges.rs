//! The edges of the verifier that the claim-by-claim tests in `tests.rs` do not reach: every
//! refusal's name, the shape errors of a token and of an allow-list, a JWKS with nothing
//! usable in it, and every way fetching the issuer's keys fails. Each test names what it
//! kills; none rests on another's state.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use serde_json::{Value, json};

use super::testkit::{
    FakeIssuer, STATIC_ISSUER, good_claims, jwks_of, keys, mint, mint_raw, static_verifier,
};
use super::{HttpsKeys, KeySource, OidcConfig, Refusal, Roots};

fn part(value: &Value) -> String {
    B64.encode(value.to_string())
}

/// Every refusal has its own stable name: two sharing one would make a metric or an answer
/// ambiguous, and an empty one would answer nothing.
#[test]
fn every_refusal_has_its_own_name() {
    let all = [
        Refusal::Malformed("x"),
        Refusal::Algorithm,
        Refusal::HeaderParameter,
        Refusal::UnknownKey,
        Refusal::KeysUnavailable,
        Refusal::BadSignature,
        Refusal::MissingClaim("x"),
        Refusal::Issuer,
        Refusal::Audience,
        Refusal::Expired,
        Refusal::NotYetValid,
        Refusal::IssuedInFuture,
        Refusal::Lifetime,
        Refusal::Repository,
        Refusal::Workflow,
        Refusal::Ref,
        Refusal::Environment,
        Refusal::Event,
        Refusal::Runner,
        Refusal::Replayed,
        Refusal::ReplayCacheFull,
    ];
    let names: std::collections::BTreeSet<_> = all.iter().map(|r| r.name()).collect();
    assert_eq!(names.len(), all.len());
    assert!(names.iter().all(|n| !n.is_empty()));
    assert!(all.iter().all(|r| !r.to_string().is_empty()));
}

#[test]
fn the_system_clock_reads_unix_seconds() {
    assert!(super::system_clock()() > 1_700_000_000);
}

/// A token whose parts are the wrong shape is `Malformed` before any key is asked for.
///
/// Kills: a header that is not an object, a `kid` that is empty, over 128 bytes or not a
/// string, a payload that is not JSON or not an object, each read as if it were fine.
#[tokio::test]
async fn the_parts_of_a_token_must_have_the_right_shape() {
    let (verifier, _) = static_verifier();
    let claims = part(&good_claims(STATIC_ISSUER));
    let header = |kid: Value| part(&json!({"alg": "RS256", "kid": kid}));
    let tokens = [
        format!("{}.{claims}.{}", part(&json!(["RS256"])), B64.encode("s")),
        format!("{}.{claims}.{}", header(json!("")), B64.encode("s")),
        format!(
            "{}.{claims}.{}",
            header(json!("k".repeat(129))),
            B64.encode("s")
        ),
        format!("{}.{claims}.{}", header(json!(7)), B64.encode("s")),
        format!(
            "{}.{}.{}",
            header(json!("k1")),
            B64.encode("not json"),
            B64.encode("s")
        ),
        format!(
            "{}.{}.{}",
            header(json!("k1")),
            part(&json!([1])),
            B64.encode("s")
        ),
    ];
    for token in tokens {
        assert!(
            matches!(verifier.verify(&token).await, Err(Refusal::Malformed(_))),
            "{token:.40}"
        );
    }
}

/// Claims that are absent or of the wrong type are refused by name, not read as empty.
///
/// Kills: a missing `ref` or `environment` let through when the allow-list names them, a
/// `jti` that is empty or over 256 bytes, an `nbf` that is not a number, an `aud` that is
/// absent or not text.
#[tokio::test]
async fn claims_that_are_absent_or_mistyped_are_refused() {
    let drop = |name: &'static str| {
        move |c: &mut Value| {
            c.as_object_mut().expect("an object").remove(name);
        }
    };
    type Edit = Box<dyn Fn(&mut Value)>;
    let cases: Vec<(&str, Edit, Refusal)> = vec![
        ("ref", Box::new(drop("ref")), Refusal::MissingClaim("ref")),
        (
            "environment",
            Box::new(drop("environment")),
            Refusal::MissingClaim("environment"),
        ),
        ("aud", Box::new(drop("aud")), Refusal::MissingClaim("aud")),
        (
            "aud type",
            Box::new(|c| c["aud"] = json!(7)),
            Refusal::MissingClaim("aud"),
        ),
        (
            "jti",
            Box::new(|c| c["jti"] = json!("")),
            Refusal::MissingClaim("jti"),
        ),
        (
            "jti long",
            Box::new(|c| c["jti"] = json!("j".repeat(257))),
            Refusal::MissingClaim("jti"),
        ),
        (
            "nbf type",
            Box::new(|c| c["nbf"] = json!("soon")),
            Refusal::MissingClaim("nbf"),
        ),
        ("iss", Box::new(drop("iss")), Refusal::MissingClaim("iss")),
    ];
    for (what, edit, expect) in cases {
        let (verifier, _) = static_verifier();
        let mut claims = good_claims(STATIC_ISSUER);
        edit(&mut claims);
        let token = mint(&keys()[0], &claims);
        assert_eq!(verifier.verify(&token).await, Err(expect), "{what}");
    }
}

fn config_with(extra: &str) -> String {
    format!(
        "[remote_operator]\nissuer = \"https://i.example\"\naudience = \"a\"\nrepository_id = \"1\"\n\
         job_workflow_ref = [\"o/r/.github/workflows/w.yml@refs/heads/main\"]\n\
         refs = [\"refs/heads/main\"]\n{extra}"
    )
}

/// An allow-list value with a stray space, empty, or the wrong shape is refused, whichever
/// key it is in: a space that is part of a value would never match a claim.
///
/// Kills: a list entry or a scalar trimmed (or left alone) instead of refused; an issuer with
/// a query, a fragment or nothing after the scheme.
#[test]
fn an_allow_list_value_with_stray_space_or_shape_is_refused() {
    for extra in [
        "repository = \" o/r\"\n",
        "repository = \"\"\n",
        "runner_environment = \"\"\n",
        "event_names = [\"push\", \"\"]\n",
        "event_names = [\" push\"]\n",
    ] {
        assert!(OidcConfig::parse(&config_with(extra)).is_err(), "{extra}");
    }
    let base = config_with("");
    for (from, to) in [
        ("\"https://i.example\"", "\"https://i.example?x=1\""),
        ("\"https://i.example\"", "\"https://i.example#x\""),
        ("\"https://i.example\"", "\"https://\""),
        ("[\"refs/heads/main\"]", "[\"refs/heads/main\", \" x\"]"),
        (
            "[\"o/r/.github/workflows/w.yml@refs/heads/main\"]",
            "[\"\"]",
        ),
        ("audience = \"a\"", "audience = \" a\""),
    ] {
        assert!(
            OidcConfig::parse(&base.replace(from, to)).is_err(),
            "{from} -> {to}"
        );
    }
    let missing = OidcConfig::from_file(std::path::Path::new("/nonexistent/ci.toml"))
        .expect_err("a file that is not there");
    assert!(
        missing.to_string().contains("/nonexistent/ci.toml"),
        "{missing}"
    );
}

/// A JWKS with no RS256 signing key in it is an error, not an empty key set that verifies
/// nothing quietly; keys of another kind, use, algorithm or with no name or number are
/// skipped, and the usable one beside them is kept.
///
/// Kills: an EC key, an `enc` key, an RS384 key, a key with no `kid` or a broken `n` taken
/// as a signing key.
#[test]
fn only_rs256_signing_keys_of_a_jwks_are_kept() {
    let good = keys()[0].jwk();
    let skipped = [
        json!({"kty": "EC", "kid": "ec", "crv": "P-256"}),
        json!({"kty": "RSA", "kid": "enc", "use": "enc", "n": "AQAB", "e": "AQAB"}),
        json!({"kty": "RSA", "kid": "rs384", "alg": "RS384", "n": "AQAB", "e": "AQAB"}),
        json!({"kty": "RSA", "n": "AQAB", "e": "AQAB"}),
        json!({"kty": "RSA", "kid": "no-n", "e": "AQAB"}),
        json!({"kty": "RSA", "kid": "bad-n", "n": "!!", "e": "AQAB"}),
    ];
    let kept = super::keys::parse_jwks(
        &json!({"keys": skipped.iter().chain([&good]).collect::<Vec<_>>()}).to_string(),
    )
    .expect("one usable key");
    assert_eq!(kept.keys().collect::<Vec<_>>(), ["k1"]);
    assert!(super::keys::parse_jwks(&json!({"keys": skipped}).to_string()).is_err());
    assert!(super::keys::parse_jwks("not json").is_err());
}

/// Every way of failing to fetch the issuer's keys is an error that names the URL, and none
/// of them yields keys.
///
/// Kills: a plain-http issuer fetched, a discovery document taken for the issuer's when it
/// names another or has no `jwks_uri` or is not JSON, a connection that never answers waiting
/// for ever, a refused connection or a bad status read as an empty answer.
#[tokio::test]
async fn every_way_of_failing_to_fetch_the_keys_is_an_error() {
    let issuer = FakeIssuer::start(jwks_of(&[&keys()[0]])).await;
    let source = |url: &str, wait: Duration| {
        HttpsKeys::new(url, issuer.roots.clone(), wait).expect("a source")
    };
    let good = source(&issuer.url, Duration::from_secs(5));
    assert!(good.jwks().await.is_ok());
    assert!(format!("{good:?}").contains("HttpsKeys"));

    let failures = [
        (
            "not https",
            source("http://localhost:1", Duration::from_secs(5)),
        ),
        (
            "refused",
            source("https://localhost:1", Duration::from_secs(5)),
        ),
        ("no host", source("https://", Duration::from_secs(5))),
    ];
    for (what, bad) in failures {
        let why = bad.jwks().await.expect_err(what);
        assert!(!why.is_empty(), "{what}: {why}");
    }
    for (what, document) in [
        ("not json", "nonsense".to_owned()),
        ("another issuer", json!({"issuer": "https://other.example", "jwks_uri": format!("{}/.well-known/jwks", issuer.url)}).to_string()),
        ("no jwks_uri", json!({"issuer": issuer.url}).to_string()),
    ] {
        issuer.set_discovery(&document);
        assert!(good.jwks().await.is_err(), "{what}");
    }
    let hung = FakeIssuer::start(jwks_of(&[&keys()[0]])).await;
    hung.set_hang();
    let slow = HttpsKeys::new(&hung.url, hung.roots.clone(), Duration::from_millis(200))
        .expect("a source");
    let why = slow.jwks().await.expect_err("it never answers");
    assert!(why.contains("timed out"), "{why}");
    let down = FakeIssuer::start(jwks_of(&[&keys()[0]])).await;
    down.set_fail(true);
    let why = HttpsKeys::new(&down.url, down.roots.clone(), Duration::from_secs(5))
        .expect("a source")
        .jwks()
        .await
        .expect_err("503");
    assert!(why.contains("503"), "{why}");
}

/// A source with no roots to trust is refused; the system's roots are read (whether the host
/// has any is the host's business).
#[test]
fn a_source_needs_roots_to_trust() {
    let none = HttpsKeys::new(
        "https://i.example",
        Roots::Given(Vec::new()),
        Duration::from_secs(1),
    );
    assert!(none.is_err());
    let _ = Arc::new(HttpsKeys::new(
        "https://i.example",
        Roots::System,
        Duration::from_secs(1),
    ));
}

/// A raw token with a signature part that is not base64url is `Malformed`.
#[tokio::test]
async fn a_signature_that_is_not_base64url_is_malformed() {
    let (verifier, _) = static_verifier();
    let token = format!(
        "{}.{}.{}",
        part(&json!({"alg": "RS256", "kid": "k1"})),
        part(&good_claims(STATIC_ISSUER)),
        "***"
    );
    assert!(matches!(
        verifier.verify(&token).await,
        Err(Refusal::Malformed(_))
    ));
    let _ = mint_raw;
}

fn verifier_for(text: &str) -> Arc<super::Verifier> {
    use super::testkit::{StaticKeys, clock};
    let (clock, _) = clock();
    Arc::new(super::Verifier::new(
        OidcConfig::parse(text).expect("a config"),
        Arc::new(StaticKeys(jwks_of(&[&keys()[0]]))),
        Duration::from_secs(30),
        clock,
    ))
}

fn allow_list(lists: &str) -> String {
    format!(
        "[remote_operator]\nissuer = \"{STATIC_ISSUER}\"\naudience = \"{}\"\nrepository_id = \"4242\"\n\
         job_workflow_ref = [\"{}\"]\n{lists}",
        super::testkit::AUDIENCE,
        super::testkit::WORKFLOW,
    )
}

/// An allow-list with only `refs`, or only `environments`, decides on that claim alone; the
/// other claim need not be in the token. A token with no `nbf` is as good as one with it, and
/// an `aud` list of one wrong audience is the wrong audience.
///
/// Kills: a check on a claim the allow-list does not list (a token with no `environment`
/// refused by a refs-only list), a required `nbf`, an audience list accepted whatever it names.
#[tokio::test]
async fn an_allow_list_decides_on_the_claims_it_lists_only() {
    let by_ref = verifier_for(&allow_list("refs = [\"refs/heads/main\"]\n"));
    let by_env = verifier_for(&allow_list("environments = [\"gamma\"]\n"));
    let mut claims = good_claims(STATIC_ISSUER);
    claims
        .as_object_mut()
        .expect("object")
        .remove("environment");
    let token = mint(&keys()[0], &claims);
    assert!(by_ref.verify(&token).await.is_ok());
    let mut claims = good_claims(STATIC_ISSUER);
    claims.as_object_mut().expect("object").remove("ref");
    claims.as_object_mut().expect("object").remove("nbf");
    let token = mint(&keys()[0], &claims);
    assert!(by_env.verify(&token).await.is_ok());
    let mut claims = good_claims(STATIC_ISSUER);
    claims["aud"] = json!(["someone-else"]);
    let token = mint(&keys()[0], &claims);
    assert_eq!(by_ref.verify(&token).await, Err(Refusal::Audience));
    assert!(format!("{by_ref:?}").contains(STATIC_ISSUER));
}

/// The fake issuer survives clients that are not clients: a connection that speaks no TLS, one
/// that ends after the handshake, and a path it does not serve (404, which is an error to the
/// verifier, not keys).
#[tokio::test]
async fn the_fake_issuer_and_the_fetch_survive_odd_clients() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let issuer = FakeIssuer::start(jwks_of(&[&keys()[0]])).await;
    let port: u16 = issuer
        .url
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .expect("a port");
    let mut plain = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    plain
        .write_all(b"not tls at all\r\n\r\n")
        .await
        .expect("write");
    let mut sink = Vec::new();
    let _ = plain.read_to_end(&mut sink).await;

    let mut store = rustls::RootCertStore::empty();
    store.add(issuer.root.clone()).expect("a root");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_root_certificates(store)
        .with_no_client_auth();
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let name = rustls::pki_types::ServerName::try_from("localhost").expect("a name");
    let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
        .expect("the handshake");
    drop(tls);
    tokio::time::sleep(Duration::from_millis(100)).await;

    issuer.set_discovery(
        &json!({"issuer": issuer.url, "jwks_uri": format!("{}/nope", issuer.url)}).to_string(),
    );
    let source = HttpsKeys::new(&issuer.url, issuer.roots.clone(), Duration::from_secs(5))
        .expect("a source");
    let why = source.jwks().await.expect_err("404");
    assert!(why.contains("404"), "{why}");
}

/// A key source the test steers: the JWKS it serves, whether it fails, and whether it holds a
/// request until released.
struct Steered {
    jwks: std::sync::Mutex<String>,
    fail: std::sync::atomic::AtomicBool,
    hold: std::sync::atomic::AtomicBool,
    release: tokio::sync::Notify,
    fetches: std::sync::atomic::AtomicUsize,
}

impl KeySource for Steered {
    fn jwks(&self) -> futures::future::BoxFuture<'_, Result<String, String>> {
        use std::sync::atomic::Ordering::SeqCst;
        Box::pin(async move {
            self.fetches.fetch_add(1, SeqCst);
            if self.hold.load(SeqCst) {
                self.release.notified().await;
            }
            if self.fail.load(SeqCst) {
                return Err("down".to_owned());
            }
            Ok(self.jwks.lock().expect("lock").clone())
        })
    }
}

fn steered(
    first: &[&super::testkit::TestKey],
) -> (
    Arc<Steered>,
    Arc<super::Verifier>,
    Arc<std::sync::atomic::AtomicU64>,
) {
    let source = Arc::new(Steered {
        jwks: std::sync::Mutex::new(jwks_of(first)),
        fail: false.into(),
        hold: false.into(),
        release: tokio::sync::Notify::new(),
        fetches: 0.into(),
    });
    let (clock, now) = super::testkit::clock();
    let config = OidcConfig::parse(&super::testkit::config_text(STATIC_ISSUER)).expect("a config");
    let verifier = Arc::new(super::Verifier::new(
        config,
        Arc::clone(&source) as Arc<dyn KeySource>,
        Duration::from_secs(30),
        clock,
    ));
    (source, verifier, now)
}

/// A token for `key`, valid at the verifier's clock `now`.
fn token_at(key: &super::testkit::TestKey, now: u64) -> String {
    let mut claims = good_claims(STATIC_ISSUER);
    claims["iat"] = now.into();
    claims["nbf"] = now.into();
    claims["exp"] = (now + 300).into();
    mint(key, &claims)
}

/// A key the issuer withdrew is refused once the key set is an hour old, though no token with
/// an unknown `kid` ever asked for a fetch; until then it still verifies; and if the issuer is
/// down at that time the keys held are kept (an outage is not a reason to refuse every deploy;
/// a withdrawn key is trusted until a fetch succeeds).
///
/// Kills: a key set that never ages (the maximum age made huge), an age that is not measured
/// from the last successful fetch, a failed refetch that empties the keys.
#[tokio::test]
async fn a_withdrawn_key_stops_verifying_within_the_maximum_key_age() {
    use std::sync::atomic::Ordering::SeqCst;
    let (source, verifier, now) = steered(&[&keys()[0]]);
    let t0 = super::testkit::NOW;
    assert!(verifier.verify(&token_at(&keys()[0], t0)).await.is_ok());
    assert_eq!(source.fetches.load(SeqCst), 1);
    *source.jwks.lock().expect("lock") = jwks_of(&[&keys()[1]]);

    now.store(t0 + 3000, SeqCst);
    assert!(
        verifier
            .verify(&token_at(&keys()[0], t0 + 3000))
            .await
            .is_ok(),
        "not old yet"
    );
    assert_eq!(source.fetches.load(SeqCst), 1, "no fetch before the age");

    source.fail.store(true, SeqCst);
    let late = t0 + 3600 + 10;
    now.store(late, SeqCst);
    assert!(
        verifier.verify(&token_at(&keys()[0], late)).await.is_ok(),
        "the issuer is down: keys kept"
    );
    assert_eq!(source.fetches.load(SeqCst), 2);

    source.fail.store(false, SeqCst);
    now.store(late + 31, SeqCst);
    assert_eq!(
        verifier.verify(&token_at(&keys()[0], late + 31)).await,
        Err(Refusal::UnknownKey),
        "the issuer is back and no longer publishes the key"
    );
    assert!(
        verifier
            .verify(&token_at(&keys()[1], late + 31))
            .await
            .is_ok()
    );
}

/// A token whose key is held is answered while a fetch for another `kid` waits on a slow issuer.
///
/// Kills: the lock held across the fetch, which stalls every token for as long as the issuer
/// takes to answer.
#[tokio::test]
async fn a_held_key_is_not_kept_waiting_by_a_slow_fetch() {
    use std::sync::atomic::Ordering::SeqCst;
    let (source, verifier, now) = steered(&[&keys()[0]]);
    let t0 = super::testkit::NOW;
    assert!(verifier.verify(&token_at(&keys()[0], t0)).await.is_ok());
    now.store(t0 + 31, SeqCst);
    source.hold.store(true, SeqCst);
    let slow = {
        let verifier = Arc::clone(&verifier);
        let token = token_at(&keys()[1], t0 + 31);
        tokio::spawn(async move { verifier.verify(&token).await })
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        while source.fetches.load(SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the second fetch starts");
    let fast = tokio::time::timeout(
        Duration::from_secs(2),
        verifier.verify(&token_at(&keys()[0], t0 + 31)),
    )
    .await;
    assert!(matches!(fast, Ok(Ok(_))), "{fast:?}");
    source.hold.store(false, SeqCst);
    source.release.notify_waiters();
    assert_eq!(slow.await.expect("joined"), Err(Refusal::UnknownKey));
}

/// A held key in a set that is old, with a fetch already under way, is used and does not wait.
#[tokio::test]
async fn an_old_key_is_used_while_its_refresh_is_under_way() {
    use std::sync::atomic::Ordering::SeqCst;
    let (source, verifier, now) = steered(&[&keys()[0]]);
    let t0 = super::testkit::NOW;
    assert!(verifier.verify(&token_at(&keys()[0], t0)).await.is_ok());
    let late = t0 + 3600 + 10;
    now.store(late, SeqCst);
    source.hold.store(true, SeqCst);
    let refresh = {
        let verifier = Arc::clone(&verifier);
        let token = token_at(&keys()[0], late);
        tokio::spawn(async move { verifier.verify(&token).await })
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        while source.fetches.load(SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the second fetch starts");
    let other = tokio::time::timeout(
        Duration::from_secs(2),
        verifier.verify(&token_at(&keys()[0], late)),
    )
    .await;
    assert!(matches!(other, Ok(Ok(_))), "{other:?}");
    source.hold.store(false, SeqCst);
    source.release.notify_waiters();
    assert!(refresh.await.expect("joined").is_ok());
}

/// The allow-list of the gamma deploy (docs/design/updates.md, section 5.1), whose job
/// runs from a `workflow_run` of the release workflow on main: its token says `workflow_run`.
const GAMMA: &str = r#"
[remote_operator]
issuer = "https://token.actions.githubusercontent.com"
audience = "kbf-gamma"
repository_id = "1234567"
repository = "komira-ai/kbf"
job_workflow_ref = ["komira-ai/kbf/.github/workflows/farm-check.yml@refs/heads/main"]
environments = ["gamma"]
event_names = ["workflow_run"]
"#;

/// The gamma allow-list accepts the token of a `workflow_run` job of `farm-check.yml` on main in the
/// `gamma` environment, and refuses the same token as a `pull_request_target` (which runs fork
/// code with secrets), a `farm-check.yml` of another branch, and another environment.
///
/// Kills: an event list that does not take `workflow_run`, one that takes any event, a workflow
/// pin that does not include the ref.
#[tokio::test]
async fn the_gamma_allow_list_takes_a_workflow_run_and_not_a_pull_request_target() {
    use super::testkit::{StaticKeys, clock};
    let (clock, _) = clock();
    let verifier = super::Verifier::new(
        OidcConfig::parse(GAMMA).expect("the gamma allow-list"),
        Arc::new(StaticKeys(jwks_of(&[&keys()[0]]))),
        Duration::from_secs(30),
        clock,
    );
    let claims = |edit: fn(&mut Value)| {
        let mut c = good_claims("https://token.actions.githubusercontent.com");
        c["aud"] = json!("kbf-gamma");
        c["repository_id"] = json!("1234567");
        c["repository"] = json!("komira-ai/kbf");
        c["job_workflow_ref"] =
            json!("komira-ai/kbf/.github/workflows/farm-check.yml@refs/heads/main");
        c["event_name"] = json!("workflow_run");
        edit(&mut c);
        mint(&keys()[0], &c)
    };
    assert!(verifier.verify(&claims(|_| {})).await.is_ok());
    type Edit = fn(&mut Value);
    let refused: [(Edit, Refusal); 4] = [
        (
            |c: &mut Value| c["event_name"] = json!("pull_request_target"),
            Refusal::Event,
        ),
        (
            |c: &mut Value| c["event_name"] = json!("push"),
            Refusal::Event,
        ),
        (
            |c: &mut Value| {
                c["job_workflow_ref"] =
                    json!("komira-ai/kbf/.github/workflows/farm-check.yml@refs/heads/topic");
            },
            Refusal::Workflow,
        ),
        (
            |c: &mut Value| c["environment"] = json!("prod"),
            Refusal::Environment,
        ),
    ];
    for (edit, why) in refused {
        assert_eq!(verifier.verify(&claims(edit)).await, Err(why));
    }
}
