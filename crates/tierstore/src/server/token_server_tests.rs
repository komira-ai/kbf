//! What the server does with a request signed by a token, through `Server::handle`.

use std::sync::Arc;

use hyper::StatusCode;

use crate::TestClock;

use super::AccessKey;
use super::ops_tests::completion;
use super::testing::{BUCKET, KEY_ID, NOW, Reply, Rig, SECRET, texts};
use super::token::tests::{CREATE, DELETE, READ, ceiling, claims, key};
use super::token::{Ceiling, Claims, Credential, MintingKey, Ops, TokenRole, forge, mint};

const MEBIBYTE: u64 = 1 << 20;

/// The minting key that tokens for jobs are made with: create and read under `jobs/`.
fn jobs_key() -> MintingKey {
    key("jobs-mint")
}

/// The minting key of the garbage collector: delete under `jobs/`.
fn gc_key() -> MintingKey {
    let gc = Ceiling {
        ops: DELETE,
        ..ceiling()
    };
    MintingKey::new("jobs-gc", &[3_u8; 32], gc).unwrap()
}

/// The minting key of small bodies: create under `jobs/`, a body of 10 bytes at most.
fn small_key() -> MintingKey {
    let small = Ceiling {
        ops: CREATE,
        max_object_bytes: Some(10),
        ..ceiling()
    };
    MintingKey::new("jobs-small", &[5_u8; 32], small).unwrap()
}

/// A server that holds the three minting keys, a read-only key, and a clock set by hand.
struct Fixture {
    rig: Rig,
    clock: Arc<TestClock>,
}

async fn fixture_with(keys: Vec<MintingKey>) -> Fixture {
    let clock = Arc::new(TestClock::new());
    clock.set(NOW);
    let shared = Arc::clone(&clock);
    let made = Rig::with(MEBIBYTE, move |config| {
        config.token_keys = keys;
        config.clock = shared;
        config.read_only_keys = vec![AccessKey::new("AKIDREADER", "reader-secret")];
    });
    Fixture {
        rig: made.await,
        clock,
    }
}

async fn fixture() -> Fixture {
    fixture_with(vec![jobs_key(), gc_key(), small_key()]).await
}

fn token(key: &MintingKey, claims: &Claims) -> Credential {
    mint(key, claims, NOW).unwrap()
}

fn create_and_read() -> Ops {
    Ops {
        create: true,
        read: true,
        delete: false,
    }
}

fn gc_claims(prefix: &str) -> Claims {
    Claims {
        role: TokenRole::Gc,
        ..claims(prefix, DELETE)
    }
}

fn uri(key: &str) -> String {
    format!("/{BUCKET}/{key}")
}

impl Fixture {
    /// Sends a request signed by the credential.
    async fn by(
        &self,
        who: &Credential,
        method: &str,
        uri: &str,
        extra: &[(&str, &str)],
        body: &[u8],
    ) -> Reply {
        let pair = (who.access_key_id.as_str(), who.secret_access_key.as_str());
        self.rig.send_as(pair, method, uri, extra, body).await
    }

    /// A create-only put by the credential.
    async fn put(&self, who: &Credential, key: &str, body: &[u8]) -> Reply {
        let create = [("if-none-match", "*")];
        self.by(who, "PUT", &uri(key), &create, body).await
    }

    async fn get(&self, who: &Credential, key: &str) -> Reply {
        self.by(who, "GET", &uri(key), &[], b"").await
    }

    /// The code of the error in the answer, or the empty text, and the status.
    fn refused(reply: &Reply) -> (u16, String) {
        (reply.status.as_u16(), reply.code())
    }
}

/// Asserts that a create token puts under its prefix and only there; that a second put of the
/// same key is refused and leaves the first body; that a put that asks to replace (`If-Match`)
/// is refused; and that it cannot read, list or delete. It catches a token that can overwrite, or
/// one that writes outside its prefix.
#[tokio::test(start_paused = true)]
async fn a_create_token_creates_under_its_prefix_and_never_overwrites() {
    let f = fixture().await;
    let who = token(&jobs_key(), &claims("jobs/a/", CREATE));
    assert_eq!(
        f.put(&who, "jobs/a/x", b"first").await.status,
        StatusCode::OK
    );
    let again = f.put(&who, "jobs/a/x", b"second").await;
    assert_eq!(
        Fixture::refused(&again),
        (412, "PreconditionFailed".to_owned())
    );
    let plain = f.by(&who, "PUT", &uri("jobs/a/x"), &[], b"third").await;
    assert_eq!(plain.status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(f.rig.get("jobs/a/x", None).await.body, "first");
    let replace = [("if-match", "\"anything\"")];
    let swap = f
        .by(&who, "PUT", &uri("jobs/a/x"), &replace, b"fourth")
        .await;
    assert_eq!(Fixture::refused(&swap), (403, "AccessDenied".to_owned()));
    assert_eq!(f.rig.get("jobs/a/x", None).await.body, "first");
    let outside = f.put(&who, "jobs/b/x", b"no").await;
    assert_eq!(Fixture::refused(&outside), (403, "AccessDenied".to_owned()));
    assert_eq!(
        f.rig.get("jobs/b/x", None).await.status,
        StatusCode::NOT_FOUND
    );
    let read = f.get(&who, "jobs/a/x").await;
    assert_eq!(Fixture::refused(&read), (403, "AccessDenied".to_owned()));
    let head = f.by(&who, "HEAD", &uri("jobs/a/x"), &[], b"").await;
    assert_eq!(head.status, StatusCode::FORBIDDEN);
    let listing = format!("/{BUCKET}?list-type=2&prefix=jobs%2Fa%2F");
    let list = f.by(&who, "GET", &listing, &[], b"").await;
    assert_eq!(list.status, StatusCode::FORBIDDEN);
    let delete = f.by(&who, "DELETE", &uri("jobs/a/x"), &[], b"").await;
    assert_eq!(delete.status, StatusCode::FORBIDDEN);
    assert_eq!(f.rig.get("jobs/a/x", None).await.body, "first");
}

/// Asserts that a create token does the four calls of a multipart upload under its prefix (start,
/// part, completion, abort) and none outside it.
#[tokio::test(start_paused = true)]
async fn a_create_token_does_multipart_under_its_prefix_only() {
    let f = fixture().await;
    let who = token(&jobs_key(), &claims("jobs/a/", CREATE));
    let started = f
        .by(&who, "POST", &uri("jobs/a/big?uploads"), &[], b"")
        .await;
    assert_eq!(started.status, StatusCode::OK);
    let id = texts(&started.body, "UploadId").concat();
    let part_uri = uri(&format!("jobs/a/big?partNumber=1&uploadId={id}"));
    let part = f.by(&who, "PUT", &part_uri, &[], b"hello").await;
    assert_eq!(part.status, StatusCode::OK);
    let etag = part.header("etag").to_owned();
    let done = completion(&[(1, etag.as_str())]);
    let end = uri(&format!("jobs/a/big?uploadId={id}"));
    let completed = f.by(&who, "POST", &end, &[], done.as_bytes()).await;
    assert_eq!(completed.status, StatusCode::OK);
    assert_eq!(f.rig.get("jobs/a/big", None).await.body, "hello");
    let second = f
        .by(&who, "POST", &uri("jobs/a/gone?uploads"), &[], b"")
        .await;
    let id = texts(&second.body, "UploadId").concat();
    let abort = uri(&format!("jobs/a/gone?uploadId={id}"));
    assert_eq!(
        f.by(&who, "DELETE", &abort, &[], b"").await.status,
        StatusCode::NO_CONTENT
    );
    let outside = f
        .by(&who, "POST", &uri("jobs/b/big?uploads"), &[], b"")
        .await;
    assert_eq!(Fixture::refused(&outside), (403, "AccessDenied".to_owned()));
    let part_outside = uri(&format!("jobs/b/big?partNumber=1&uploadId={id}"));
    let stray = f.by(&who, "PUT", &part_outside, &[], b"x").await;
    assert_eq!(stray.status, StatusCode::FORBIDDEN);
    let abort_outside = uri(&format!("jobs/b/big?uploadId={id}"));
    let stray = f.by(&who, "DELETE", &abort_outside, &[], b"").await;
    assert_eq!(stray.status, StatusCode::FORBIDDEN);
}

/// Asserts that a read token gets, heads and lists inside its prefix, that a list returns the keys
/// under the prefix of the listing and no other, and that it cannot create, delete or upload.
#[tokio::test(start_paused = true)]
async fn a_read_token_reads_and_lists_inside_its_prefix() {
    let f = fixture().await;
    for name in ["jobs/a/1", "jobs/a/sub/2", "jobs/b/1"] {
        assert_eq!(
            f.rig.put(name, name.as_bytes()).await.status,
            StatusCode::OK
        );
    }
    let who = token(&jobs_key(), &claims("jobs/a/", READ));
    let got = f.get(&who, "jobs/a/1").await;
    assert_eq!(
        (got.status, got.body.as_str()),
        (StatusCode::OK, "jobs/a/1")
    );
    let head = f.by(&who, "HEAD", &uri("jobs/a/sub/2"), &[], b"").await;
    assert_eq!(head.status, StatusCode::OK);
    let missing = f.get(&who, "jobs/a/none").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    let listing = |query: &str| format!("/{BUCKET}?list-type=2&{query}");
    let all = f
        .by(&who, "GET", &listing("prefix=jobs%2Fa%2F"), &[], b"")
        .await;
    assert_eq!(all.status, StatusCode::OK);
    assert_eq!(texts(&all.body, "Key"), ["jobs/a/1", "jobs/a/sub/2"]);
    let deeper = f
        .by(&who, "GET", &listing("prefix=jobs%2Fa%2Fsub"), &[], b"")
        .await;
    assert_eq!(texts(&deeper.body, "Key"), ["jobs/a/sub/2"]);
    assert_eq!(
        f.put(&who, "jobs/a/new", b"x").await.status,
        StatusCode::FORBIDDEN
    );
    let delete = f.by(&who, "DELETE", &uri("jobs/a/1"), &[], b"").await;
    assert_eq!(delete.status, StatusCode::FORBIDDEN);
    let start = f
        .by(&who, "POST", &uri("jobs/a/big?uploads"), &[], b"")
        .await;
    assert_eq!(start.status, StatusCode::FORBIDDEN);
    assert_eq!(f.rig.get("jobs/a/1", None).await.status, StatusCode::OK);
}

/// Asserts that a listing whose prefix is not inside the token's is refused and not filtered: the
/// shorter prefixes, none at all, a sibling, and every spelling of `..` that would climb out.
#[tokio::test(start_paused = true)]
async fn a_list_prefix_outside_the_token_prefix_is_denied() {
    let f = fixture().await;
    f.rig.put("jobs/a/1", b"1").await;
    f.rig.put("jobs/b/1", b"2").await;
    let who = token(&jobs_key(), &claims("jobs/a/", READ));
    let denied = [
        "",
        "prefix=",
        "prefix=jobs%2F",
        "prefix=jobs",
        "prefix=jobs%2Fa",
        "prefix=jobs%2Fb%2F",
        "prefix=jobs%2Fa%2F..%2Fb%2F",
        "prefix=jobs%2Fa%2F%2e%2e%2Fb%2F",
        "prefix=jobs%2Fa%2F%2E%2E%2Fb%2F",
        "prefix=jobs%2Fa%2F..",
        "prefix=jobs%2Fa%2F.%2Fb",
        "prefix=jobs%2Fa%2F%00",
        "prefix=_journal%2F",
        "prefix=_tiering%2F",
        "prefix=%2F",
    ];
    for query in denied {
        let listing = format!("/{BUCKET}?list-type=2&{query}");
        let list = f.by(&who, "GET", &listing, &[], b"").await;
        assert_eq!(
            Fixture::refused(&list),
            (403, "AccessDenied".to_owned()),
            "{query}"
        );
        assert!(!list.body.contains("jobs/b/1"), "{query}");
    }
    let start_after = "prefix=jobs%2Fa%2F&start-after=jobs%2Fa%2F0";
    let listing = format!("/{BUCKET}?list-type=2&{start_after}");
    let inside = f.by(&who, "GET", &listing, &[], b"").await;
    assert_eq!(texts(&inside.body, "Key"), ["jobs/a/1"]);
}

/// Asserts that no spelling of a key reaches outside the prefix of a read token, after the path
/// is decoded: a literal `..`, the encoded and mixed-case encodings of it, a `.` segment, an
/// encoded slash, the prefix without its closing slash, a sibling that shares its first bytes, a
/// NUL; that a doubly encoded `..` is a literal key inside the prefix and does not climb; that a
/// full-width dot and a decomposed accent are not folded into the ASCII or composed form (the
/// match is on bytes); and that each denied request is answered before the object is looked up.
#[tokio::test(start_paused = true)]
async fn no_spelling_of_a_key_escapes_the_prefix() {
    let f = fixture().await;
    f.rig.put("jobs/b/secret", b"SECRET").await;
    f.rig.put("jobs/ab/secret", b"SECRET").await;
    f.rig.put("jobs/e%CC%81/secret", b"SECRET").await;
    f.rig.put("cas/x", b"SECRET").await;
    let who = token(&jobs_key(), &claims("jobs/a/", READ));
    let denied = [
        "jobs/a/../b/secret",
        "jobs/a/%2e%2e/b/secret",
        "jobs/a/%2E%2E/b/secret",
        "jobs/a/.%2e/b/secret",
        "jobs/a/%2e./b/secret",
        "jobs/a/..%2Fb/secret",
        "jobs/a/%2e%2e%2fb/secret",
        "jobs/a/./../b/secret",
        "jobs/a/./b",
        "jobs/a/%2e/b",
        "jobs/a/../../cas/x",
        "jobs/b/secret",
        "jobs%2Fb%2Fsecret",
        "jobs/ab/secret",
        "jobs/a",
        "jobs/a/..",
        "jobs/a/%2e%2e%2e/b/secret",
        "jobs/a/x%00",
        "jobs/a/x%0A",
        "cas/x",
        "_journal/p1/0000000000000001",
        "_tiering/usage",
    ];
    for name in denied {
        for method in ["GET", "HEAD"] {
            let reply = f.by(&who, method, &uri(name), &[], b"").await;
            assert_eq!(Fixture::refused(&reply).0, 403, "{method} {name}");
            assert!(!reply.body.contains("SECRET"), "{name}");
        }
    }
    let literal = [
        "jobs/a/%252e%252e/b/secret",
        "jobs/a/%EF%BC%8E%EF%BC%8E/b/secret",
    ];
    for name in literal {
        let reply = f.get(&who, name).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{name}");
        assert!(!reply.body.contains("SECRET"), "{name}");
    }
    let accented = token(&jobs_key(), &claims("jobs/\u{e9}/", READ));
    let decomposed = f.get(&accented, "jobs/e%CC%81/secret").await;
    assert_eq!(decomposed.status, StatusCode::FORBIDDEN);
    assert!(!decomposed.body.contains("SECRET"));
    let composed = f.get(&accented, "jobs/%C3%A9/secret").await;
    assert_eq!(composed.status, StatusCode::NOT_FOUND);
}

/// Asserts the same of a create token and of a gc token: an escaping key cannot be put, started or
/// deleted, and nothing outside the prefix changes.
#[tokio::test(start_paused = true)]
async fn escaping_keys_cannot_be_written_or_deleted() {
    let f = fixture().await;
    f.rig.put("jobs/b/keep", b"keep").await;
    let create = token(&jobs_key(), &claims("jobs/a/", CREATE));
    let gc = token(&gc_key(), &gc_claims("jobs/a/"));
    for name in [
        "jobs/a/../b/new",
        "jobs/a/%2e%2e/b/new",
        "jobs/a/./new",
        "jobs/b/new",
    ] {
        assert_eq!(
            f.put(&create, name, b"x").await.status,
            StatusCode::FORBIDDEN,
            "{name}"
        );
        let start = f
            .by(&create, "POST", &uri(&format!("{name}?uploads")), &[], b"")
            .await;
        assert_eq!(start.status, StatusCode::FORBIDDEN, "{name}");
    }
    for name in [
        "jobs/a/../b/keep",
        "jobs/a/%2e%2e/b/keep",
        "jobs/a/./../b/keep",
        "jobs/b/keep",
    ] {
        let delete = f.by(&gc, "DELETE", &uri(name), &[], b"").await;
        assert_eq!(delete.status, StatusCode::FORBIDDEN, "{name}");
    }
    assert_eq!(f.rig.get("jobs/b/keep", None).await.body, "keep");
    assert_eq!(
        f.rig.get("jobs/b/new", None).await.status,
        StatusCode::NOT_FOUND
    );
}

/// Asserts that delete is the gc role's: a create token and a read token are refused, a gc token
/// deletes under its prefix and not outside it, and a gc token with only the delete operation
/// cannot put or read.
#[tokio::test(start_paused = true)]
async fn only_a_gc_token_deletes_and_only_under_its_prefix() {
    let f = fixture().await;
    f.rig.put("jobs/a/x", b"x").await;
    f.rig.put("jobs/b/x", b"y").await;
    let create = token(&jobs_key(), &claims("jobs/a/", create_and_read()));
    let gc = token(&gc_key(), &gc_claims("jobs/a/"));
    let refused = f.by(&create, "DELETE", &uri("jobs/a/x"), &[], b"").await;
    assert_eq!(Fixture::refused(&refused), (403, "AccessDenied".to_owned()));
    assert_eq!(f.rig.get("jobs/a/x", None).await.status, StatusCode::OK);
    let outside = f.by(&gc, "DELETE", &uri("jobs/b/x"), &[], b"").await;
    assert_eq!(outside.status, StatusCode::FORBIDDEN);
    assert_eq!(f.rig.get("jobs/b/x", None).await.status, StatusCode::OK);
    assert_eq!(
        f.put(&gc, "jobs/a/new", b"z").await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(f.get(&gc, "jobs/a/x").await.status, StatusCode::FORBIDDEN);
    let deleted = f.by(&gc, "DELETE", &uri("jobs/a/x"), &[], b"").await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert_eq!(
        f.rig.get("jobs/a/x", None).await.status,
        StatusCode::NOT_FOUND
    );
    let abort_start = f
        .by(&create, "POST", &uri("jobs/a/u?uploads"), &[], b"")
        .await;
    assert_eq!(abort_start.status, StatusCode::OK);
}

/// Asserts that the keys of the proxy are out of reach: a token for them is not accepted even
/// with a good MAC (the prefix is checked again when it is verified), and a token for a prefix of
/// its own gets `AccessDenied` for them.
#[tokio::test(start_paused = true)]
async fn the_reserved_prefixes_are_never_reachable() {
    let f = fixture().await;
    let wide = key("jobs-mint");
    for prefix in [
        "_journal/",
        "_journal/p1/",
        "_tiering/",
        "_tiering/usage",
        "_",
    ] {
        let forged = forge(&wide, &claims(prefix, create_and_read()));
        let reply = f.get(&forged, &format!("{prefix}x")).await;
        let want = (403, "InvalidAccessKeyId".to_owned());
        assert_eq!(Fixture::refused(&reply), want, "{prefix}");
        let reply = f.put(&forged, &format!("{prefix}x"), b"x").await;
        assert_eq!(Fixture::refused(&reply), want, "{prefix}");
    }
    let who = token(&jobs_key(), &claims("jobs/a/", create_and_read()));
    for name in [
        "_journal/p1/0000000000000001",
        "_tiering/lease",
        "_tiering/usage",
    ] {
        let reply = f.get(&who, name).await;
        assert_eq!(
            Fixture::refused(&reply),
            (403, "AccessDenied".to_owned()),
            "{name}"
        );
    }
}

/// Asserts the expiry rule against the clock of the server: good a millisecond before the second
/// named by `not after`, `ExpiredToken` from it on, and still `ExpiredToken` minutes later: unlike a signature, which has 15 minutes of
/// skew, a token has no grace.
#[tokio::test(start_paused = true)]
async fn a_token_is_refused_from_its_not_after_second_with_no_grace() {
    let f = fixture().await;
    let who = token(&jobs_key(), &claims("jobs/a/", create_and_read()));
    assert_eq!(f.get(&who, "jobs/a/x").await.status, StatusCode::NOT_FOUND);
    f.clock.set(NOW + 59_999);
    assert_eq!(f.get(&who, "jobs/a/x").await.status, StatusCode::NOT_FOUND);
    for late in [60_000, 60_001, 5 * 60_000, 14 * 60_000] {
        f.clock.set(NOW + late);
        let reply = f.get(&who, "jobs/a/x").await;
        assert_eq!(
            Fixture::refused(&reply),
            (403, "ExpiredToken".to_owned()),
            "{late}"
        );
    }
    let put = f.put(&who, "jobs/a/x", b"x").await;
    assert_eq!(put.code(), "ExpiredToken");
    assert_eq!(
        f.rig.get("jobs/a/x", None).await.status,
        StatusCode::NOT_FOUND
    );
}

/// Asserts that ids that do not verify are all answered `InvalidAccessKeyId`, which says nothing
/// of what was wrong: a changed byte in the claims (a widened prefix, say), a changed MAC, a key
/// id the server does not hold, a key that was retired; and that the same id with the secret of
/// another token is `SignatureDoesNotMatch`. It catches a server that accepts a claim the MAC
/// does not cover.
#[tokio::test(start_paused = true)]
async fn ids_that_do_not_verify_are_unknown_keys_and_a_wrong_secret_is_a_bad_signature() {
    let f = fixture().await;
    f.rig.put("jobs/a/x", b"x").await;
    let who = token(&jobs_key(), &claims("jobs/a/", READ));
    assert_eq!(f.get(&who, "jobs/a/x").await.status, StatusCode::OK);
    let other = token(&jobs_key(), &claims("jobs/a/", create_and_read()));
    let wrong_secret = Credential {
        secret_access_key: other.secret_access_key.clone(),
        ..who.clone()
    };
    let reply = f.get(&wrong_secret, "jobs/a/x").await;
    assert_eq!(
        Fixture::refused(&reply),
        (403, "SignatureDoesNotMatch".to_owned())
    );
    let mut bad = Vec::new();
    for at in [5, who.access_key_id.len() / 2, who.access_key_id.len() - 3] {
        let mut chars: Vec<char> = who.access_key_id.chars().collect();
        chars[at] = if chars[at] == 'A' { 'B' } else { 'A' };
        let id: String = chars.into_iter().collect();
        bad.push(Credential {
            access_key_id: id,
            ..who.clone()
        });
    }
    let stranger = MintingKey::new("jobs-other", &[9_u8; 32], ceiling()).unwrap();
    bad.push(token(&stranger, &claims("jobs/a/", READ)));
    for credential in bad {
        let reply = f.get(&credential, "jobs/a/x").await;
        let want = (403, "InvalidAccessKeyId".to_owned());
        assert_eq!(
            Fixture::refused(&reply),
            want,
            "{}",
            credential.access_key_id
        );
    }
    let junk = Credential {
        access_key_id: "tt1AAAA".to_owned(),
        secret_access_key: "s".to_owned(),
    };
    assert_eq!(f.get(&junk, "jobs/a/x").await.code(), "InvalidAccessKeyId");
}

/// Asserts the rotation: with the new key and the old key both held, a token of each is good; when
/// the old key is dropped from the server, the tokens of the old key are refused and those of the
/// new key are not.
#[tokio::test(start_paused = true)]
async fn a_retired_minting_key_is_refused_after_rotation() {
    let (old, new) = (key("jobs-old"), key("jobs-new"));
    let before = fixture_with(vec![new.clone(), old.clone()]).await;
    let (by_old, by_new) = (
        token(&old, &claims("jobs/a/", READ)),
        token(&new, &claims("jobs/a/", READ)),
    );
    assert_eq!(
        before.get(&by_old, "jobs/a/x").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        before.get(&by_new, "jobs/a/x").await.status,
        StatusCode::NOT_FOUND
    );
    let after = fixture_with(vec![new]).await;
    let reply = after.get(&by_old, "jobs/a/x").await;
    assert_eq!(
        Fixture::refused(&reply),
        (403, "InvalidAccessKeyId".to_owned())
    );
    assert_eq!(
        after.get(&by_new, "jobs/a/x").await.status,
        StatusCode::NOT_FOUND
    );
    let none = fixture_with(Vec::new()).await;
    assert_eq!(
        none.get(&by_new, "jobs/a/x").await.status,
        StatusCode::FORBIDDEN
    );
}

/// Asserts that a token the key's holder made with a good MAC, but beyond the ceiling of the key,
/// is refused by the server, in each dimension: a prefix of another area (`cas/`, with the jobs
/// key), an operation the key does not have (delete with the jobs key, create with the gc key),
/// and a lifetime past the ceiling's. The same key's token inside the ceiling works. It catches a
/// verifier that stops at the MAC.
#[tokio::test(start_paused = true)]
async fn a_token_beyond_the_ceiling_of_its_key_is_refused_by_the_server() {
    let f = fixture().await;
    f.rig.put("cas/x", b"cas").await;
    let hour = NOW / 1000 + 3600;
    let beyond = [
        forge(&jobs_key(), &claims("cas/", READ)),
        forge(&jobs_key(), &gc_claims("jobs/a/")),
        forge(&gc_key(), &claims("jobs/a/", create_and_read())),
        forge(
            &jobs_key(),
            &Claims {
                not_after: hour + 1,
                ..claims("jobs/a/", READ)
            },
        ),
    ];
    for who in beyond {
        for name in ["cas/x", "jobs/a/x"] {
            let reply = f.get(&who, name).await;
            let want = (403, "InvalidAccessKeyId".to_owned());
            assert_eq!(Fixture::refused(&reply), want, "{name}");
        }
    }
    let at_limit = Claims {
        not_after: hour,
        ..claims("jobs/a/", READ)
    };
    let fine = forge(&jobs_key(), &at_limit);
    assert_eq!(f.get(&fine, "jobs/a/x").await.status, StatusCode::NOT_FOUND);
}

/// Asserts that the size limit of a key's ceiling holds each body: a put of the limit is stored,
/// one byte more is `EntityTooLarge` and stores nothing, and so is a part over it.
#[tokio::test(start_paused = true)]
async fn the_size_limit_of_the_ceiling_bounds_a_body() {
    let f = fixture().await;
    let who = token(&small_key(), &claims("jobs/a/", CREATE));
    assert_eq!(
        f.put(&who, "jobs/a/ten", b"0123456789").await.status,
        StatusCode::OK
    );
    let over = f.put(&who, "jobs/a/eleven", b"0123456789A").await;
    assert_eq!(Fixture::refused(&over), (413, "EntityTooLarge".to_owned()));
    assert_eq!(
        f.rig.get("jobs/a/eleven", None).await.status,
        StatusCode::NOT_FOUND
    );
    let started = f
        .by(&who, "POST", &uri("jobs/a/big?uploads"), &[], b"")
        .await;
    let id = texts(&started.body, "UploadId").concat();
    let part = uri(&format!("jobs/a/big?partNumber=1&uploadId={id}"));
    let big = f.by(&who, "PUT", &part, &[], b"0123456789A").await;
    assert_eq!(big.status, StatusCode::PAYLOAD_TOO_LARGE);
    let jobs = token(&jobs_key(), &claims("jobs/a/", create_and_read()));
    let long = vec![b'z'; 4096];
    assert_eq!(
        f.put(&jobs, "jobs/a/long", &long).await.status,
        StatusCode::OK
    );
}

/// Asserts that the static keys are as before beside the minting keys: the full key puts, gets,
/// lists and deletes anywhere; a read-only key reads and is refused a write; a key id of no key
/// is `InvalidAccessKeyId`.
#[tokio::test(start_paused = true)]
async fn static_keys_work_unchanged_beside_tokens() {
    let f = fixture().await;
    assert_eq!(f.rig.put("cas/a", b"a").await.status, StatusCode::OK);
    assert_eq!(f.rig.get("cas/a", None).await.body, "a");
    assert_eq!(
        texts(&f.rig.list("prefix=cas%2F").await.body, "Key"),
        ["cas/a"]
    );
    let reader = Credential {
        access_key_id: "AKIDREADER".to_owned(),
        secret_access_key: "reader-secret".to_owned(),
    };
    assert_eq!(f.get(&reader, "cas/a").await.body, "a");
    assert_eq!(
        f.put(&reader, "cas/b", b"b").await.status,
        StatusCode::FORBIDDEN
    );
    let full = Credential {
        access_key_id: KEY_ID.to_owned(),
        secret_access_key: SECRET.to_owned(),
    };
    let tokens_prefix = f.put(&full, "jobs/a/x", b"x").await;
    assert_eq!(tokens_prefix.status, StatusCode::OK);
    let delete = f.by(&full, "DELETE", &uri("cas/a"), &[], b"").await;
    assert_eq!(delete.status, StatusCode::NO_CONTENT);
    let stranger = Credential {
        access_key_id: "AKIDNOBODY".to_owned(),
        secret_access_key: "x".to_owned(),
    };
    assert_eq!(f.get(&stranger, "cas/a").await.code(), "InvalidAccessKeyId");
}

/// Asserts what a token may do with the bucket itself: head it (an S3 client's setup does), and
/// nothing else: CreateBucket and a root listing are refused.
#[tokio::test(start_paused = true)]
async fn a_token_may_head_the_bucket_and_do_nothing_else_with_it() {
    let f = fixture().await;
    let who = token(&jobs_key(), &claims("jobs/a/", create_and_read()));
    let bucket = format!("/{BUCKET}");
    assert_eq!(
        f.by(&who, "HEAD", &bucket, &[], b"").await.status,
        StatusCode::OK
    );
    assert_eq!(
        f.by(&who, "PUT", &bucket, &[], b"").await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.by(&who, "GET", "/", &[], b"").await.status,
        StatusCode::FORBIDDEN
    );
    let copy = [("x-amz-copy-source", "/tier-bucket/jobs/b/x")];
    let copied = f.by(&who, "PUT", &uri("jobs/a/copy"), &copy, b"").await;
    assert_ne!(copied.status, StatusCode::OK);
}
