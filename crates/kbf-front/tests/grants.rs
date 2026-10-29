//! Input read grants: the token a task reads its Action's inputs with.
//!
//! Every test runs on an injected clock and fixed digests, so none waits or reads the
//! wall.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use kbf_auth::{AuthenticationMetadata, Authenticator, authorize};
use kbf_front::grants::{
    Claims, ClaimsError, GrantError, GrantKey, Grants, KeyError, MIN_SECRET_LEN, Scope,
};
use kbf_types::{Digest, DigestFunction};
use tonic::Code;
use tonic::codegen::http;

const BUILD: &str = "00000000000000070000000000000003";
const NOT_AFTER: u64 = 1_000;

fn digest(n: u8, size: u64) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], size)
}

fn key(fill: u8) -> GrantKey {
    GrantKey::from_secret(&[fill; MIN_SECRET_LEN]).expect("a long enough secret")
}

/// Grants whose clock is the returned counter, set to `now`.
fn grants(now: u64) -> (Arc<Grants>, Arc<AtomicU64>) {
    let clock = Arc::new(AtomicU64::new(now));
    let read = Arc::clone(&clock);
    let grants = Grants::new(key(1), Arc::new(move || read.load(Ordering::SeqCst)));
    (Arc::new(grants), clock)
}

/// The Action, its Command, two inputs and a digest that is none of them.
struct Rig {
    action: Digest,
    command: Digest,
    inputs: [Digest; 2],
    stranger: Digest,
}

fn rig() -> Rig {
    Rig {
        action: digest(1, 140),
        command: digest(2, 90),
        inputs: [digest(3, 5_000), digest(4, 0)],
        stranger: digest(9, 12),
    }
}

impl Rig {
    fn scope(&self) -> Scope {
        Scope::new(self.action, self.command, self.inputs)
    }
}

fn text_of(grants: &Grants, action: Digest, not_after: u64) -> String {
    grants
        .issue(BUILD, action, not_after)
        .expect("a valid build id")
        .expose()
        .to_owned()
}

/// Catches: a token that does not survive its own verification (a field dropped or
/// reordered in the text), a scope that leaves out the Action, the Command or an input
/// (an empty input file included), and a check that reads only the first digest.
#[test]
fn a_token_for_this_action_reads_its_command_and_inputs() {
    let (grants, _) = grants(10);
    let r = rig();
    let text = text_of(&grants, r.action, NOT_AFTER);
    let claims = grants.verify(&text).expect("a fresh token");
    assert_eq!(
        claims,
        Claims {
            build: BUILD.to_owned(),
            action: r.action,
            not_after: NOT_AFTER
        }
    );
    let scope = r.scope();
    scope
        .check(
            &claims,
            &[r.action, r.command, r.inputs[0], r.inputs[1], r.inputs[0]],
        )
        .expect("every digest of the closure");
    assert!(scope.allows(&r.command));
    scope.check(&claims, &[]).expect("nothing asked");
}

/// Catches: a scope that allows anything, one that compares the hash and not the size,
/// and a batch that is allowed because its first digest is.
#[test]
fn a_digest_outside_the_closure_is_refused() {
    let (grants, _) = grants(10);
    let r = rig();
    let claims = grants
        .verify(&text_of(&grants, r.action, NOT_AFTER))
        .expect("a fresh token");
    let scope = r.scope();
    let same_hash_other_size = digest(3, 5_001);
    for out in [r.stranger, same_hash_other_size] {
        assert!(!scope.allows(&out), "{out}");
        let e = scope.check(&claims, &[out]).expect_err("out of scope");
        assert_eq!(e.code(), Code::PermissionDenied, "{out}");
        let e = scope
            .check(&claims, &[r.inputs[0], out])
            .expect_err("one of two out of scope");
        assert_eq!(e.code(), Code::PermissionDenied, "{out}");
    }
}

/// Catches: a scope check that ignores whose grant it is (a token of one Action reading
/// another's inputs), and a token whose Action field can be changed without the MAC
/// noticing.
#[test]
fn a_token_of_another_action_is_refused() {
    let (grants, _) = grants(10);
    let r = rig();
    let other = Rig {
        action: digest(5, 140),
        ..rig()
    };
    let claims = grants
        .verify(&text_of(&grants, r.action, NOT_AFTER))
        .expect("a fresh token");
    let e = other
        .scope()
        .check(&claims, &[other.command])
        .expect_err("another action's scope");
    assert_eq!(e.code(), Code::PermissionDenied);

    let text = text_of(&grants, r.action, NOT_AFTER);
    let swapped = text.replace(&r.action.to_string(), &other.action.to_string());
    assert_ne!(swapped, text);
    assert_eq!(grants.verify(&swapped), Err(GrantError::BadMac));
}

/// Catches: expiry read as `>` (the grant valid at its own second), as never, or from
/// the issue time; and a clock read once at construction.
#[test]
fn an_expired_token_is_refused_at_not_after() {
    let (grants, clock) = grants(10);
    let text = text_of(&grants, rig().action, NOT_AFTER);
    for ok in [0, 10, NOT_AFTER - 1] {
        clock.store(ok, Ordering::SeqCst);
        grants.verify(&text).unwrap_or_else(|e| panic!("{ok}: {e}"));
    }
    for late in [NOT_AFTER, NOT_AFTER + 1, u64::MAX] {
        clock.store(late, Ordering::SeqCst);
        assert_eq!(grants.verify(&text), Err(GrantError::Expired), "{late}");
    }
}

/// Catches: a MAC compared on a prefix, a field left out of the MAC, a token of another
/// key accepted, a text that is not five parts or not of version 1, and uppercase hex
/// taken as the same MAC.
#[test]
fn a_flipped_mac_bit_is_refused() {
    let (grants, _) = grants(10);
    let text = text_of(&grants, rig().action, NOT_AFTER);
    grants.verify(&text).expect("untouched");

    // One character of every position of the text, changed, is refused: the MAC, and
    // every field the MAC covers.
    for at in 0..text.len() {
        let mut bytes = text.clone().into_bytes();
        bytes[at] = if bytes[at] == b'0' { b'1' } else { b'0' };
        let changed = String::from_utf8(bytes).expect("ascii");
        let e = grants.verify(&changed).expect_err(&changed);
        assert!(
            matches!(e, GrantError::BadMac | GrantError::Malformed),
            "{at}: {e}"
        );
    }
    let (body, mac) = text.rsplit_once('.').expect("a MAC");
    let mut flipped = mac.to_owned().into_bytes();
    flipped[0] ^= 1;
    let flipped = format!("{body}.{}", String::from_utf8(flipped).expect("ascii"));
    assert_eq!(grants.verify(&flipped), Err(GrantError::BadMac));
    let later = text.replace(&format!(".{NOT_AFTER}."), &format!(".{}.", NOT_AFTER + 1));
    assert_eq!(grants.verify(&later), Err(GrantError::BadMac));

    let other_key = Grants::new(key(2), Arc::new(|| 10));
    assert_eq!(other_key.verify(&text), Err(GrantError::BadMac));

    let upper = format!("{body}.{}", mac.to_uppercase());
    assert_eq!(grants.verify(&upper), Err(GrantError::Malformed));
    let short = format!("{body}.{}", &mac[..mac.len() - 2]);
    assert_eq!(grants.verify(&short), Err(GrantError::Malformed));
    let version2 = text.replacen("v1.", "v2.", 1);
    assert_eq!(grants.verify(&version2), Err(GrantError::BadMac));
    for bad in ["", "v1", "v1.a.b.c", &format!("{text}.x")] {
        assert_eq!(grants.verify(bad), Err(GrantError::Malformed), "{bad:?}");
    }
}

/// Catches: a revocation that does not refuse, one that refuses every build, one
/// forgotten before its `not_after`, one kept after it, and an entry that is never
/// dropped.
#[test]
fn a_denied_build_is_refused_until_not_after() {
    let (grants, clock) = grants(10);
    let r = rig();
    let text = text_of(&grants, r.action, NOT_AFTER);
    let other_build = grants
        .issue("another", r.action, NOT_AFTER)
        .expect("a valid build id");
    grants.revoke(BUILD, NOT_AFTER);
    assert_eq!(grants.verify(&text), Err(GrantError::Revoked));
    grants
        .verify(other_build.expose())
        .expect("another build is not revoked");
    clock.store(NOT_AFTER - 1, Ordering::SeqCst);
    assert_eq!(grants.verify(&text), Err(GrantError::Revoked));
    assert_eq!(grants.revoked_len(), 1);

    // At `not_after` the grant is expired whatever the deny set says, and the next
    // revocation drops the entry that ended.
    clock.store(NOT_AFTER, Ordering::SeqCst);
    assert_eq!(grants.verify(&text), Err(GrantError::Expired));
    grants.revoke("another", NOT_AFTER + 50);
    assert_eq!(grants.revoked_len(), 1);

    // A longer revocation of the same build is kept, a shorter one does not shorten it.
    clock.store(10, Ordering::SeqCst);
    let held = grants.revoked_len();
    grants.revoke("stale", 5);
    assert_eq!(
        grants.revoked_len(),
        held,
        "an ended revocation is not kept"
    );
    grants.revoke(BUILD, 20);
    grants.revoke(BUILD, 15);
    clock.store(16, Ordering::SeqCst);
    let later = text_of(&grants, r.action, NOT_AFTER);
    assert_eq!(grants.verify(&later), Err(GrantError::Revoked));
    clock.store(20, Ordering::SeqCst);
    grants.verify(&later).expect("the revocation ended at 20");
}

fn authenticated(
    grants: &Arc<Grants>,
    header: &str,
) -> Result<Arc<AuthenticationMetadata>, tonic::Status> {
    let mut request = http::Request::new(());
    request.headers_mut().insert(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_str(header).expect("a header value"),
    );
    let (parts, ()) = request.into_parts();
    let authenticator = grants.authenticator();
    futures::executor::block_on(authenticator.authenticate(&parts))
}

/// Catches: a job-facing listener that lets a grant write blobs, ask FindMissing, read
/// the action cache, execute or read capabilities; one that lets a call with no grant
/// read; and an authenticator that accepts a token the grants refuse.
#[test]
fn a_write_or_findmissing_call_with_a_token_is_refused() {
    let (grants, clock) = grants(10);
    let r = rig();
    let text = text_of(&grants, r.action, NOT_AFTER);
    let md = authenticated(&grants, &format!("Bearer {text}")).expect("a good token");
    let az = Grants::authorizers();
    let call = |a: &Arc<dyn kbf_auth::Authorizer>, md: &AuthenticationMetadata| {
        futures::executor::block_on(authorize(&**a, md, "call", "main"))
    };
    call(&az.cas_get, &md).expect("a grant reads the CAS");
    for (name, a) in [
        ("capabilities", &az.capabilities),
        ("cas_put", &az.cas_put),
        ("cas_find_missing", &az.cas_find_missing),
        ("ac_get", &az.ac_get),
        ("execute", &az.execute),
    ] {
        let e = call(a, &md).expect_err(name);
        assert_eq!(e.code(), Code::PermissionDenied, "{name}");
    }
    let e = call(&az.cas_get, &AuthenticationMetadata::default()).expect_err("no grant");
    assert_eq!(e.code(), Code::PermissionDenied);

    // The claims ride the metadata to the handler that checks each digest.
    assert_eq!(
        Claims::of(&md).expect("claims"),
        grants.verify(&text).expect("valid")
    );
    assert_eq!(Claims::of(&AuthenticationMetadata::default()), None);

    // The authenticator is the grants' verdict.
    clock.store(NOT_AFTER, Ordering::SeqCst);
    let e = authenticated(&grants, &format!("Bearer {text}")).expect_err("expired");
    assert_eq!(e.code(), Code::Unauthenticated);
    assert_eq!(e.message(), "the read grant has expired");
}

/// Catches: a `Debug` or message that carries the token or the key, which would put them
/// in any log line that formats the value, and public metadata that carries the token.
#[test]
fn the_token_is_never_logged() {
    let (grants, _) = grants(10);
    let r = rig();
    let token = grants.issue(BUILD, r.action, NOT_AFTER).expect("valid");
    let text = token.expose().to_owned();
    let md = authenticated(&grants, &format!("Bearer {text}")).expect("a good token");
    let shown = format!(
        "{token:?} {:?} {grants:?} {md:?} {} {:?}",
        key(7),
        md.public_display(),
        Claims::of(&md)
    );
    assert!(!shown.contains(&text), "{shown}");
    assert!(
        !shown.contains(text.rsplit('.').next().expect("mac")),
        "{shown}"
    );
    assert!(shown.contains("Token(<redacted>)"), "{shown}");
    assert!(shown.contains("GrantKey(<redacted>)"), "{shown}");
    assert!(!shown.contains("7, 7"), "{shown}");
    for e in [GrantError::Malformed, GrantError::BadMac] {
        assert!(!format!("{e} {e:?}").contains(&text));
    }
}

/// Catches: a secret too short to be one accepted, and one that is not.
#[test]
fn a_secret_under_the_minimum_is_refused() {
    for len in [0, 1, MIN_SECRET_LEN - 1] {
        assert_eq!(
            GrantKey::from_secret(&vec![5; len]).map(|_| ()),
            Err(KeyError::TooShort { got: len })
        );
    }
    GrantKey::from_secret(&[5; MIN_SECRET_LEN]).expect("the minimum");
    GrantKey::from_secret(&[5; 100]).expect("longer");
    assert!(
        KeyError::TooShort { got: 3 }
            .to_string()
            .contains("3 bytes")
    );
}

/// Catches: a build id that would break the text's fields (a dot) or is empty or long
/// accepted, and one with a legitimate character refused.
#[test]
fn a_build_id_that_cannot_ride_the_text_is_refused() {
    let (grants, _) = grants(10);
    let a = rig().action;
    for bad in ["", "a.b", "a b", "a/b", "é", &"x".repeat(65)] {
        assert_eq!(
            grants.issue(bad, a, NOT_AFTER).map(|_| ()),
            Err(ClaimsError::Build(bad.to_owned())),
            "{bad:?}"
        );
    }
    for good in ["a", "A-9_z", &"x".repeat(64)] {
        let t = grants.issue(good, a, NOT_AFTER).expect(good);
        assert_eq!(grants.verify(t.expose()).expect("opens").build, good);
    }
    assert!(ClaimsError::Build("a.b".into()).to_string().contains("a.b"));
}

/// Catches: the system clock not read (a grant that expires at once or never), and
/// claims in metadata of another shape read as a grant.
#[test]
fn the_system_clock_is_read_and_foreign_metadata_is_no_grant() {
    let grants = Grants::system(key(1));
    let a = rig().action;
    let live = grants.issue(BUILD, a, u64::MAX).expect("valid");
    grants.verify(live.expose()).expect("not expired");
    let past = grants.issue(BUILD, a, 1).expect("valid");
    assert_eq!(grants.verify(past.expose()), Err(GrantError::Expired));

    for private in [
        serde_json::json!("grant"),
        serde_json::json!({"grant": 1}),
        serde_json::json!({"grant": {"build": "b"}}),
        serde_json::json!({"grant": {"build": "b", "action": "nope", "not_after": 1}}),
        serde_json::json!({"grant": {"build": 1, "action": "x", "not_after": 1}}),
    ] {
        let md = AuthenticationMetadata::new(None, Some(private));
        assert_eq!(Claims::of(&md), None);
    }
}
