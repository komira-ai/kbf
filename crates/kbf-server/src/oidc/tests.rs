//! The verifier against a fake issuer: a TLS server on loopback that serves a discovery
//! document and a JWKS of keys generated in the test ([`super::testkit`]).
//!
//! Each refusal below is paired with the check that produces it, and the mutant that was
//! planted (the check removed or loosened) to see the test go red is named in the doc
//! comment. A refusal test asserts the exact [`Refusal`], not only `is_err`, so a mutant
//! that lets the token fail some other check does not hide.

use std::sync::atomic::Ordering;

use serde_json::{Value, json};

use super::testkit::{
    AUDIENCE, FakeIssuer, NOW, WORKFLOW, good_claims, jwks_of, keys, mint, mint_raw, mint_with,
    verifier,
};
use super::{OidcConfig, Refusal};

async fn issuer() -> FakeIssuer {
    let k = keys();
    FakeIssuer::start(jwks_of(&[&k[0]])).await
}

/// A claim set valid for the fake issuer at `url`, mutated.
async fn refused_mutating(mutate: impl FnOnce(&mut Value)) -> Refusal {
    let issuer = issuer().await;
    let (verifier, _) = verifier(&issuer);
    let mut claims = good_claims(&issuer.url);
    mutate(&mut claims);
    let token = mint(&keys()[0], &claims);
    verifier
        .verify(&token)
        .await
        .expect_err("the token is refused")
}

/// Proves the happy path, so every refusal below is a refusal of one change and not of
/// a fixture that never worked. Mutants: `parse` rejecting RS256, the signature check
/// inverted, any claim check inverted (each turns this red).
#[tokio::test]
async fn a_good_token_passes_and_names_its_run() {
    let issuer = issuer().await;
    let (verifier, _) = verifier(&issuer);
    let token = mint(&keys()[0], &good_claims(&issuer.url));
    let who = verifier.verify(&token).await.expect("accepted");
    assert_eq!(who.job_workflow_ref, WORKFLOW);
    assert_eq!(who.git_ref.as_deref(), Some("refs/heads/main"));
    assert_eq!(issuer.jwks_requests(), 1);
}

/// `iss` must be the configured issuer exactly, trailing slash included.
/// Mutant: the `iss` comparison removed.
#[tokio::test]
async fn a_wrong_issuer_is_refused() {
    let a = refused_mutating(|c| c["iss"] = json!("https://token.example")).await;
    assert_eq!(a, Refusal::Issuer);
    let issuer = issuer().await;
    let (verifier, _) = verifier(&issuer);
    let claims = good_claims(&format!("{}/", issuer.url));
    let token = mint(&keys()[0], &claims);
    assert_eq!(verifier.verify(&token).await, Err(Refusal::Issuer));
}

/// `aud` is accepted for the kbf audience and nothing else: a different one, a list that
/// also names another, and a missing one. Mutants: the `aud` check removed (first), or
/// loosened to "contains" (second).
#[tokio::test]
async fn a_wrong_audience_is_refused() {
    assert_eq!(
        refused_mutating(|c| c["aud"] = json!("sts.amazonaws.com")).await,
        Refusal::Audience
    );
    assert_eq!(
        refused_mutating(|c| c["aud"] = json!([AUDIENCE, "sts.amazonaws.com"])).await,
        Refusal::Audience
    );
    assert_eq!(
        refused_mutating(|c| {
            c.as_object_mut().expect("object").remove("aud");
        })
        .await,
        Refusal::MissingClaim("aud")
    );
}

/// A one-element audience list is the same audience.
#[tokio::test]
async fn an_audience_list_of_one_is_accepted() {
    let issuer = issuer().await;
    let (verifier, _) = verifier(&issuer);
    let mut claims = good_claims(&issuer.url);
    claims["aud"] = json!([AUDIENCE]);
    verifier
        .verify(&mint(&keys()[0], &claims))
        .await
        .expect("accepted");
}

/// Another repository (by id, and by name when the file names it) is refused.
/// Mutants: `repository_id` unchecked; `repository` unchecked.
#[tokio::test]
async fn a_wrong_repository_is_refused() {
    assert_eq!(
        refused_mutating(|c| c["repository_id"] = json!("1")).await,
        Refusal::Repository
    );
    assert_eq!(
        refused_mutating(|c| c["repository"] = json!("someone/else")).await,
        Refusal::Repository
    );
}

/// The workflow actually running must be the deploy workflow at the allowed ref: another
/// workflow file, the same file at another ref, and a prefix of the allowed value.
/// Mutants: `job_workflow_ref` unchecked; compared with `starts_with`.
#[tokio::test]
async fn a_wrong_workflow_is_refused() {
    for other in [
        "komira-ai/komira-build-farm/.github/workflows/ci.yml@refs/heads/main",
        "komira-ai/komira-build-farm/.github/workflows/gamma.yml@refs/heads/topic",
        "komira-ai/komira-build-farm/.github/workflows/gamma.yml@refs/heads/main-evil",
        "komira-ai/komira-build-farm/.github/workflows/gamma.yml",
    ] {
        assert_eq!(
            refused_mutating(|c| c["job_workflow_ref"] = json!(other)).await,
            Refusal::Workflow,
            "{other}"
        );
    }
}

/// A run from another branch, and from another environment, is refused; both are
/// configured, so both must match. Mutants: `ref` unchecked; `environment` unchecked.
#[tokio::test]
async fn a_wrong_ref_or_environment_is_refused() {
    assert_eq!(
        refused_mutating(|c| c["ref"] = json!("refs/heads/topic")).await,
        Refusal::Ref
    );
    assert_eq!(
        refused_mutating(|c| c["environment"] = json!("scratch")).await,
        Refusal::Environment
    );
    assert_eq!(
        refused_mutating(|c| {
            c.as_object_mut().expect("object").remove("environment");
        })
        .await,
        Refusal::MissingClaim("environment")
    );
}

/// A pull request never deploys, and neither does a scheduled or other event; a
/// self-hosted runner is refused unless the file allows it.
/// Mutants: `event_name` unchecked; `runner_environment` unchecked.
#[tokio::test]
async fn a_wrong_event_or_runner_is_refused() {
    for event in [
        "pull_request",
        "pull_request_target",
        "schedule",
        "workflow_run",
    ] {
        assert_eq!(
            refused_mutating(|c| c["event_name"] = json!(event)).await,
            Refusal::Event,
            "{event}"
        );
    }
    assert_eq!(
        refused_mutating(|c| c["runner_environment"] = json!("self-hosted")).await,
        Refusal::Runner
    );
}

/// `workflow_dispatch` is allowed beside `push`.
#[tokio::test]
async fn a_workflow_dispatch_is_accepted() {
    let issuer = issuer().await;
    let (verifier, _) = verifier(&issuer);
    let mut claims = good_claims(&issuer.url);
    claims["event_name"] = json!("workflow_dispatch");
    verifier
        .verify(&mint(&keys()[0], &claims))
        .await
        .expect("accepted");
}

/// Times, against a clock the test sets: expired beyond the 60 s leeway, still accepted
/// inside it; `nbf` and `iat` in the future likewise; a lifetime over the limit; `exp`
/// not after `iat`. Mutants: `exp` unchecked (or leeway added instead of subtracted);
/// `nbf` unchecked; `iat` unchecked; the lifetime limit unchecked.
#[tokio::test]
async fn the_times_are_checked_with_a_leeway() {
    let now = NOW as i64;
    assert_eq!(
        refused_mutating(|c| {
            c["iat"] = json!(now - 400);
            c["exp"] = json!(now - 60);
        })
        .await,
        Refusal::Expired
    );
    assert_eq!(
        refused_mutating(|c| c["nbf"] = json!(now + 61)).await,
        Refusal::NotYetValid
    );
    assert_eq!(
        refused_mutating(|c| c["iat"] = json!(now + 61)).await,
        Refusal::IssuedInFuture
    );
    assert_eq!(
        refused_mutating(|c| c["exp"] = json!(now + 601)).await,
        Refusal::Lifetime
    );
    assert_eq!(
        refused_mutating(|c| c["exp"] = json!(now)).await,
        Refusal::Lifetime
    );
    assert_eq!(
        refused_mutating(|c| {
            c.as_object_mut().expect("object").remove("exp");
        })
        .await,
        Refusal::MissingClaim("exp")
    );
    assert_eq!(
        refused_mutating(|c| c["exp"] = json!(1.5e9)).await,
        Refusal::MissingClaim("exp")
    );
    // Inside the leeway: expired 59 s ago, not valid for another 59 s.
    let issuer = issuer().await;
    let (verifier, _) = verifier(&issuer);
    let mut claims = good_claims(&issuer.url);
    claims["iat"] = json!(now - 300);
    claims["exp"] = json!(now - 59);
    claims["nbf"] = json!(now + 59);
    verifier
        .verify(&mint(&keys()[0], &claims))
        .await
        .expect("within the leeway");
}

/// A token that has been used is refused the second time, even though its signature and
/// claims still hold; one with another `jti` passes; and the entry is forgotten after the
/// token could no longer pass anyway. Mutants: the `jti` check removed; the entry never
/// pruned (the last assertion would then hold a stale entry, so the cap test below
/// catches it too).
#[tokio::test]
async fn a_replayed_token_is_refused() {
    let issuer = issuer().await;
    let (verifier, now) = verifier(&issuer);
    let claims = good_claims(&issuer.url);
    let token = mint(&keys()[0], &claims);
    verifier.verify(&token).await.expect("first use");
    assert_eq!(verifier.verify(&token).await, Err(Refusal::Replayed));
    let other = mint(&keys()[0], &good_claims(&issuer.url));
    verifier.verify(&other).await.expect("another jti");
    now.store(NOW + 300 + 61, Ordering::SeqCst);
    assert_eq!(verifier.verify(&token).await, Err(Refusal::Expired));
    let mut fresh = good_claims(&issuer.url);
    fresh["iat"] = json!(NOW + 400);
    fresh["nbf"] = json!(NOW + 400);
    fresh["exp"] = json!(NOW + 700);
    verifier
        .verify(&mint(&keys()[0], &fresh))
        .await
        .expect("a fresh token");
    assert_eq!(
        verifier.seen.lock().expect("seen").len(),
        1,
        "the two spent entries are pruned"
    );
}

/// A token refused for another reason does not spend its `jti`: fixing the claim and
/// presenting it again (same jti) is the first use.
#[tokio::test]
async fn a_refused_token_does_not_spend_its_jti() {
    let issuer = issuer().await;
    let (verifier, _) = verifier(&issuer);
    let mut claims = good_claims(&issuer.url);
    claims["event_name"] = json!("pull_request");
    assert_eq!(
        verifier.verify(&mint(&keys()[0], &claims)).await,
        Err(Refusal::Event)
    );
    claims["event_name"] = json!("push");
    verifier
        .verify(&mint(&keys()[0], &claims))
        .await
        .expect("first accepted use");
}

/// A full replay cache refuses new tokens instead of forgetting a live one.
/// Mutant: the cap removed (the insert would evict or grow).
#[tokio::test]
async fn a_full_replay_cache_fails_closed() {
    let issuer = issuer().await;
    let (verifier, _) = verifier(&issuer);
    {
        let mut seen = verifier.seen.lock().expect("seen");
        for n in 0..super::MAX_REMEMBERED_JTIS {
            seen.insert(format!("old-{n}"), NOW + 1000);
        }
    }
    let token = mint(&keys()[0], &good_claims(&issuer.url));
    assert_eq!(verifier.verify(&token).await, Err(Refusal::ReplayCacheFull));
}

/// Signatures: another key under the same `kid`, a payload changed after signing, a
/// truncated and an empty signature. Mutant: signature verification skipped (each of
/// these then passes or fails on a later check).
#[tokio::test]
async fn a_bad_signature_is_refused() {
    let issuer = issuer().await;
    let (verifier, _) = verifier(&issuer);
    let k = keys();
    let claims = good_claims(&issuer.url);
    let forged = mint_with(&k[1], &json!({"alg": "RS256", "kid": "k1"}), &claims);
    assert_eq!(verifier.verify(&forged).await, Err(Refusal::BadSignature));
    let signed = mint(&k[0], &claims);
    let parts: Vec<&str> = signed.split('.').collect();
    let mut other = good_claims(&issuer.url);
    other["ref"] = json!("refs/heads/main");
    other["sha"] = json!("ffffffffffffffffffffffffffffffffffffffff");
    let payload = base64_of(&other);
    let tampered = format!("{}.{payload}.{}", parts[0], parts[2]);
    assert_eq!(verifier.verify(&tampered).await, Err(Refusal::BadSignature));
    let header = json!({"alg": "RS256", "kid": "k1"});
    assert_eq!(
        verifier
            .verify(&mint_raw(&header, &claims, &[1, 2, 3]))
            .await,
        Err(Refusal::BadSignature)
    );
    assert_eq!(
        verifier.verify(&mint_raw(&header, &claims, &[])).await,
        Err(Refusal::BadSignature)
    );
}

fn base64_of(value: &Value) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string())
}

/// The algorithm is fixed: `none`, HS256 keyed with the issuer's public modulus (the
/// classic confusion), `RS384`, a lower-case name and a missing `alg` are all refused
/// before any key is used. Mutant: `alg` taken from the header (HS256 or none accepted).
#[tokio::test]
async fn any_algorithm_but_rs256_is_refused() {
    let issuer = issuer().await;
    let (verifier, _) = verifier(&issuer);
    let claims = good_claims(&issuer.url);
    let none = mint_raw(&json!({"alg": "none", "kid": "k1"}), &claims, &[]);
    assert_eq!(verifier.verify(&none).await, Err(Refusal::Algorithm));
    let header = json!({"alg": "HS256", "kid": "k1"});
    let input = format!("{}.{}", base64_of(&header), base64_of(&claims));
    let secret = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, keys()[0].n());
    let mac = ring::hmac::sign(&secret, input.as_bytes());
    let hs256 = mint_raw(&header, &claims, mac.as_ref());
    assert_eq!(verifier.verify(&hs256).await, Err(Refusal::Algorithm));
    for alg in [
        json!("RS384"),
        json!("rs256"),
        json!(["RS256"]),
        Value::Null,
    ] {
        let t = mint_with(&keys()[0], &json!({"alg": alg, "kid": "k1"}), &claims);
        assert_eq!(verifier.verify(&t).await, Err(Refusal::Algorithm), "{alg}");
    }
    let no_alg = mint_with(&keys()[0], &json!({"kid": "k1"}), &claims);
    assert_eq!(verifier.verify(&no_alg).await, Err(Refusal::Algorithm));
    assert_eq!(
        issuer.jwks_requests(),
        0,
        "refused before any key was fetched"
    );
}

/// A header that tells the verifier where to find a key, or what it must understand, is
/// refused. Mutant: the forbidden-header list emptied.
#[tokio::test]
async fn key_locating_headers_are_refused() {
    let issuer = issuer().await;
    let (verifier, _) = verifier(&issuer);
    let claims = good_claims(&issuer.url);
    for extra in [
        ("jku", json!("https://attacker.example/jwks")),
        ("x5u", json!("https://attacker.example/cert")),
        ("jwk", json!({"kty": "RSA"})),
        ("x5c", json!(["AAAA"])),
        ("crit", json!(["exp"])),
    ] {
        let mut header = json!({"alg": "RS256", "kid": "k1"});
        header[extra.0] = extra.1;
        let t = mint_with(&keys()[0], &header, &claims);
        assert_eq!(
            verifier.verify(&t).await,
            Err(Refusal::HeaderParameter),
            "{}",
            extra.0
        );
    }
}

/// Garbage in every shape is `Malformed`, never a panic or a fetch.
#[tokio::test]
async fn malformed_tokens_are_refused() {
    let issuer = issuer().await;
    let (verifier, _) = verifier(&issuer);
    let good = mint(&keys()[0], &good_claims(&issuer.url));
    let padded = format!("{}=", good.split('.').next().expect("header"));
    for bad in [
        "",
        "a",
        "a.b",
        "a.b.c.d",
        "....",
        &"x".repeat(super::jwt::MAX_TOKEN_BYTES + 1),
        &format!("{padded}.{}.{}", "e30", "e30"),
        "bm90IGpzb24.e30.e30",
    ] {
        assert!(
            matches!(verifier.verify(bad).await, Err(Refusal::Malformed(_))),
            "{bad:.20}"
        );
    }
    assert_eq!(issuer.jwks_requests(), 0);
}

/// An unknown `kid` makes one refetch and is refused; a second unknown `kid` inside the
/// interval does not refetch; after the interval it does. Mutants: no refetch at all
/// (the rotation test fails); no rate limit (the second count is 3).
#[tokio::test]
async fn an_unknown_kid_refetches_once_per_interval() {
    let issuer = issuer().await;
    let (verifier, now) = verifier(&issuer);
    let k = keys();
    verifier
        .verify(&mint(&k[0], &good_claims(&issuer.url)))
        .await
        .expect("the first fetch");
    assert_eq!(issuer.jwks_requests(), 1);
    let unknown = mint_with(
        &k[2],
        &json!({"alg": "RS256", "kid": "nope"}),
        &good_claims(&issuer.url),
    );
    assert_eq!(verifier.verify(&unknown).await, Err(Refusal::UnknownKey));
    assert_eq!(
        issuer.jwks_requests(),
        1,
        "inside the interval of the first fetch"
    );
    now.store(NOW + 31, Ordering::SeqCst);
    let mut claims = good_claims(&issuer.url);
    claims["iat"] = json!(NOW + 31);
    claims["exp"] = json!(NOW + 300);
    claims["nbf"] = json!(NOW + 31);
    let unknown = mint_with(&k[2], &json!({"alg": "RS256", "kid": "nope"}), &claims);
    assert_eq!(verifier.verify(&unknown).await, Err(Refusal::UnknownKey));
    assert_eq!(issuer.jwks_requests(), 2);
    assert_eq!(verifier.verify(&unknown).await, Err(Refusal::UnknownKey));
    assert_eq!(issuer.jwks_requests(), 2, "rate limited again");
}

/// Key rotation: the issuer publishes a new key, a token signed with it is accepted once
/// the interval allows a refetch, and the old key stays valid while published.
#[tokio::test]
async fn a_rotated_key_is_picked_up() {
    let issuer = issuer().await;
    let (verifier, now) = verifier(&issuer);
    let k = keys();
    verifier
        .verify(&mint(&k[0], &good_claims(&issuer.url)))
        .await
        .expect("old key");
    issuer.set_jwks(jwks_of(&[&k[0], &k[2]]));
    now.store(NOW + 31, Ordering::SeqCst);
    let mut claims = good_claims(&issuer.url);
    claims["iat"] = json!(NOW + 31);
    claims["exp"] = json!(NOW + 300);
    claims["nbf"] = json!(NOW + 31);
    verifier
        .verify(&mint(&k[2], &claims))
        .await
        .expect("new key");
    verifier
        .verify(&mint(&k[0], &good_claims(&issuer.url)))
        .await
        .expect("old key still");
}

/// A key the issuer withdrew stops verifying after the next refetch (the cache is
/// replaced, not merged). Mutant: new keys merged into the old set.
#[tokio::test]
async fn a_withdrawn_key_stops_verifying() {
    let issuer = issuer().await;
    let (verifier, now) = verifier(&issuer);
    let k = keys();
    verifier
        .verify(&mint(&k[0], &good_claims(&issuer.url)))
        .await
        .expect("old key");
    issuer.set_jwks(jwks_of(&[&k[2]]));
    now.store(NOW + 31, Ordering::SeqCst);
    let mut claims = good_claims(&issuer.url);
    claims["iat"] = json!(NOW + 31);
    claims["exp"] = json!(NOW + 300);
    claims["nbf"] = json!(NOW + 31);
    verifier
        .verify(&mint(&k[2], &claims))
        .await
        .expect("new key, after a refetch");
    let mut again = claims.clone();
    again["jti"] = json!("another");
    assert_eq!(
        verifier.verify(&mint(&k[0], &again)).await,
        Err(Refusal::UnknownKey)
    );
}

/// A failed fetch with keys held keeps verifying with them; a failed first fetch fails
/// closed and says why (503 at the route). Mutants: the cache cleared on a failed fetch;
/// a failed first fetch treated as "no keys, accept".
#[tokio::test]
async fn a_failed_fetch_keeps_the_keys_it_has_and_fails_closed_without() {
    let issuer = issuer().await;
    let (verifier, now) = verifier(&issuer);
    let k = keys();
    issuer.set_fail(true);
    assert_eq!(
        verifier
            .verify(&mint(&k[0], &good_claims(&issuer.url)))
            .await,
        Err(Refusal::KeysUnavailable)
    );
    issuer.set_fail(false);
    now.store(NOW + 31, Ordering::SeqCst);
    let mut claims = good_claims(&issuer.url);
    claims["iat"] = json!(NOW + 31);
    claims["exp"] = json!(NOW + 300);
    claims["nbf"] = json!(NOW + 31);
    verifier
        .verify(&mint(&k[0], &claims))
        .await
        .expect("keys fetched");
    issuer.set_fail(true);
    now.store(NOW + 100, Ordering::SeqCst);
    let mut unknown = claims.clone();
    unknown["iat"] = json!(NOW + 100);
    unknown["exp"] = json!(NOW + 300);
    unknown["jti"] = json!("x");
    assert_eq!(
        verifier
            .verify(&mint_with(
                &k[2],
                &json!({"alg": "RS256", "kid": "nope"}),
                &unknown
            ))
            .await,
        Err(Refusal::UnknownKey),
        "the fetch failed, the held keys do not know this kid"
    );
    let mut known = unknown.clone();
    known["jti"] = json!("y");
    verifier
        .verify(&mint(&k[0], &known))
        .await
        .expect("the held key still verifies");
}

/// The discovery document's `jwks_uri` must be on the issuer's origin: a document that
/// points keys at another host yields no keys, even when that host serves the right keys
/// under a certificate the verifier trusts. Mutant: the origin comparison removed (the
/// token then verifies against the other host's keys).
#[tokio::test]
async fn a_jwks_uri_off_the_issuers_origin_is_not_followed() {
    let issuer = FakeIssuer::start(jwks_of(&[&keys()[1]])).await;
    let elsewhere = FakeIssuer::start(jwks_of(&[&keys()[0]])).await;
    issuer.served.lock().expect("state").jwks_uri =
        Some(format!("{}/.well-known/jwks", elsewhere.url));
    let (super::Roots::Given(mut roots), super::Roots::Given(more)) =
        (issuer.roots.clone(), elsewhere.roots.clone())
    else {
        unreachable!("the fake issuers give their certificates");
    };
    roots.extend(more);
    let source = super::HttpsKeys::new(
        &issuer.url,
        super::Roots::Given(roots),
        std::time::Duration::from_secs(5),
    )
    .expect("a source");
    let (clock, _) = super::testkit::clock();
    let config = OidcConfig::parse(&super::testkit::config_text(&issuer.url)).expect("config");
    let verifier = super::Verifier::new(
        config,
        std::sync::Arc::new(source),
        std::time::Duration::from_secs(30),
        clock,
    );
    let token = mint(&keys()[0], &good_claims(&issuer.url));
    assert_eq!(verifier.verify(&token).await, Err(Refusal::KeysUnavailable));
    assert_eq!(elsewhere.jwks_requests(), 0);
    assert_eq!(issuer.jwks_requests(), 0);
}

/// A server whose certificate the roots do not vouch for yields no keys.
#[tokio::test]
async fn an_untrusted_issuer_certificate_yields_no_keys() {
    let issuer = issuer().await;
    let other = FakeIssuer::start(jwks_of(&[&keys()[0]])).await;
    let source = super::HttpsKeys::new(
        &issuer.url,
        other.roots.clone(),
        std::time::Duration::from_secs(5),
    )
    .expect("a source");
    let (clock, _) = super::testkit::clock();
    let config = OidcConfig::parse(&super::testkit::config_text(&issuer.url)).expect("config");
    let verifier = super::Verifier::new(
        config,
        std::sync::Arc::new(source),
        std::time::Duration::from_secs(30),
        clock,
    );
    let token = mint(&keys()[0], &good_claims(&issuer.url));
    assert_eq!(verifier.verify(&token).await, Err(Refusal::KeysUnavailable));
}

// The allow-list file.

fn config_with(extra: &str) -> String {
    format!("[remote_operator]\n{extra}")
}

const FULL: &str = "issuer = \"https://i.example\"\naudience = \"a\"\nrepository_id = \"1\"\n\
                    job_workflow_ref = [\"o/r/.github/workflows/w.yml@refs/heads/main\"]\n\
                    refs = [\"refs/heads/main\"]\n";

/// Every value that decides who is let in is required: dropping each one is refused,
/// and so is an unknown key, a non-numeric repository id, an http issuer, a trailing
/// slash, and a pull_request event. Mutant: a default for any required key.
#[test]
fn the_allow_list_requires_every_deciding_value() {
    assert!(OidcConfig::parse(&config_with(FULL)).is_ok());
    for line in FULL.lines() {
        let without = FULL.replace(&format!("{line}\n"), "");
        let why = OidcConfig::parse(&config_with(&without))
            .expect_err(line)
            .to_string();
        assert!(
            why.contains("required") || why.contains("refs"),
            "{line}: {why}"
        );
    }
    for bad in [
        format!("{FULL}surprise = 1\n"),
        FULL.replace("\"1\"", "\"one\""),
        FULL.replace("https://i.example", "http://i.example"),
        FULL.replace("https://i.example", "https://i.example/"),
        format!("{FULL}event_names = [\"pull_request\"]\n"),
        format!("{FULL}event_names = []\n"),
        format!("{FULL}max_token_lifetime_secs = 0\n"),
        format!("{FULL}max_token_lifetime_secs = 86400\n"),
        format!("{FULL}frontend_image_repository = \"r.example/x:latest\"\n"),
        FULL.replace("\"a\"", "\" a\""),
    ] {
        assert!(OidcConfig::parse(&config_with(&bad)).is_err(), "{bad}");
    }
    assert!(OidcConfig::parse("").is_err(), "no table at all");
    let envs = FULL.replace("refs = [\"refs/heads/main\"]", "environments = [\"gamma\"]");
    assert!(
        OidcConfig::parse(&config_with(&envs)).is_ok(),
        "an environment suffices"
    );
}

/// The defaults that remain are the safe ones.
#[test]
fn the_allow_list_defaults_are_narrow() {
    let c = OidcConfig::parse(&config_with(FULL)).expect("config");
    assert_eq!(c.event_names, ["push", "workflow_dispatch"]);
    assert_eq!(c.runner_environment, "github-hosted");
    assert_eq!(c.max_lifetime_secs, 600);
    assert_eq!(c.frontend_image_repository, None);
}

/// A `[remote_operator]` table sits in a `kbf.toml` beside other tables, which are ignored
/// (a misspelt key inside the table is still refused, above).
#[test]
fn the_table_may_sit_beside_other_tables() {
    let text = format!("[server]\nanything = 1\n\n{}", config_with(FULL));
    assert!(
        OidcConfig::parse(&text).is_ok(),
        "other tables are not this table's business"
    );
}
