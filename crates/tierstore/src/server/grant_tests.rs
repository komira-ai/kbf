//! The read grant: the token, its check, the check of a call against the names it lists, and the
//! grant key files.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine as _;
use hyper::Request;
use hyper::http::request::Parts;
use kbf_testkit::dir::TestDir;

use crate::TestClock;
use crate::server::testing::{BUCKET, NOW};

use super::*;

const ID: &str = "build-grant";
const SECRET: &str = "grant-secret-0123456789";
const FLAG_TEXT: &str = "--grant-key-file";

/// Ten minutes after the clock of the tests.
const EXPIRES: u64 = NOW / 1000 + 600;

/// The settings of a server for `bucket` with the grant key `ID`, and its clock, set to `NOW`.
fn server(bucket: &str) -> (ServerConfig, Arc<TestClock>) {
    let clock = Arc::new(TestClock::new());
    clock.set(NOW);
    let mut config = ServerConfig::new(bucket, "full", "full-secret", clock.clone());
    config.grant_keys = vec![AccessKey::new(ID, SECRET)];
    (config, clock)
}

/// The head of a request that carries these values of the grant header, one header for each.
fn carrying(values: &[&[u8]]) -> Parts {
    let mut builder = Request::builder().uri("/b/k");
    for value in values {
        builder = builder.header(GRANT_HEADER, *value);
    }
    builder.body(()).unwrap().into_parts().0
}

fn carrying_token(token: &str) -> Parts {
    carrying(&[token.as_bytes()])
}

/// The body of a grant of `BUCKET` that ends at `EXPIRES` and lists `names` (joined by line feeds).
fn body_of(names: &str) -> String {
    format!("{MAGIC}\n{ID}\n{BUCKET}\n{EXPIRES}\n{names}")
}

/// A token for `body` signed with the secret of the grant key, whatever the body is.
fn signed(body: &str) -> String {
    seal(SECRET, body.as_bytes())
}

fn refusal_of_token(config: &ServerConfig, token: &str) -> Refusal {
    verify(config, &carrying_token(token)).unwrap_err()
}

fn names_of(signed: &Signed) -> Vec<String> {
    match &signed.role {
        Role::Grant(names) => names.to_vec(),
        other => panic!("not a grant: {other:?}"),
    }
}

/// Asserts that a minted token is accepted by the server check with the role of a grant that
/// holds the names sorted and once each, and that the order and the repeats of the names given do
/// not change the token.
#[test]
fn a_minted_token_is_accepted_with_its_names_sorted_and_once_each() {
    let key = AccessKey::new(ID, SECRET);
    let (config, _) = server(BUCKET);
    let token = mint_grant(&key, BUCKET, EXPIRES, ["cas/l/b", "cas/b/a", "cas/l/b"]).unwrap();
    let accepted = verify(&config, &carrying_token(&token)).unwrap();
    assert_eq!(accepted.payload, UNSIGNED);
    assert!(accepted.presign.is_none());
    assert_eq!(names_of(&accepted), ["cas/b/a", "cas/l/b"]);
    let sorted = mint_grant(&key, BUCKET, EXPIRES, ["cas/b/a", "cas/l/b"]).unwrap();
    assert_eq!(token, sorted);
    let owned = vec!["cas/l/b".to_owned(), "cas/b/a".to_owned()];
    assert_eq!(token, mint_grant(&key, BUCKET, EXPIRES, owned).unwrap());
}

/// Asserts the byte form of the token: the header value is the URL-safe base64 of the body, a
/// dot, and the URL-safe base64 of the HMAC-SHA256 of the body under the secret, all without
/// padding.
#[test]
fn the_token_is_the_body_and_its_hmac_in_base64url() {
    let key = AccessKey::new(ID, SECRET);
    let token = mint_grant(&key, BUCKET, EXPIRES, ["cas/a"]).unwrap();
    let (body, tag) = token.split_once('.').unwrap();
    let body = URL_SAFE_NO_PAD.decode(body).unwrap();
    let text = format!("{MAGIC}\n{ID}\n{BUCKET}\n{EXPIRES}\ncas/a");
    assert_eq!(String::from_utf8(body.clone()).unwrap(), text);
    let tag = URL_SAFE_NO_PAD.decode(tag).unwrap();
    assert_eq!(tag.len(), 32);
    let mut mac = keyed(SECRET.as_bytes());
    mac.update(&body);
    assert!(mac.verify_slice(&tag).is_ok());
    assert!(!token.contains(['=', '+', '/']));
}

/// Asserts what a grant cannot be made of: a key with an empty or odd id or secret, a bucket that
/// is empty or has a control character, no name, too many names (4096 is the most), and a name
/// that a request cannot carry or that is the proxy's; and a token over the size limit.
#[test]
fn a_grant_that_the_server_would_refuse_is_not_made() {
    let key = AccessKey::new(ID, SECRET);
    let mint = |key: &AccessKey, bucket: &str, names: &[&str]| {
        mint_grant(key, bucket, EXPIRES, names.iter().copied())
    };
    for odd in [
        AccessKey::new("", SECRET),
        AccessKey::new("a/b", SECRET),
        AccessKey::new(ID, ""),
    ] {
        assert_eq!(mint(&odd, BUCKET, &["a"]), Err(GrantError::Key));
    }
    for bucket in ["", "a\nb", "a\tb"] {
        assert_eq!(mint(&key, bucket, &["a"]), Err(GrantError::Bucket));
    }
    assert_eq!(mint(&key, BUCKET, &[]), Err(GrantError::NoKeys));
    let long = "a".repeat(1025);
    for name in [
        "",
        "a/../b",
        "a/./b",
        "a\nb",
        "a\tb",
        "_journal/p1/x",
        "_tiering/x",
        long.as_str(),
    ] {
        let refused = mint(&key, BUCKET, &["fine", name]).unwrap_err();
        assert!(
            matches!(&refused, GrantError::Name { name: bad, .. } if bad == name),
            "{name:?}: {refused:?}"
        );
        assert!(refused.to_string().contains("cannot be granted"));
    }
    let many: Vec<String> = (0..=MAX_GRANT_KEYS).map(|i| format!("k{i:05}")).collect();
    let refused = mint_grant(&key, BUCKET, EXPIRES, &many).unwrap_err();
    assert_eq!(refused, GrantError::TooManyKeys { count: 4097 });
    assert!(mint_grant(&key, BUCKET, EXPIRES, &many[1..]).is_ok());
    let big: Vec<String> = (0..100)
        .map(|i| format!("{i:04}{}", "x".repeat(996)))
        .collect();
    let refused = mint_grant(&key, BUCKET, EXPIRES, &big).unwrap_err();
    assert!(
        matches!(refused, GrantError::TooLarge { len } if len > MAX_TOKEN_BYTES),
        "{refused:?}"
    );
    assert!(refused.to_string().contains("bytes long"));
    for refused in [
        GrantError::Key,
        GrantError::Bucket,
        GrantError::NoKeys,
        GrantError::TooManyKeys { count: 1 },
    ] {
        assert!(!refused.to_string().contains(SECRET));
    }
}

/// Asserts the checks of the shape of a request before any signature: two grant headers, a
/// header that is not text, a token over the limit (and one at the limit, which is malformed
/// only), no dot, a part that is not unpadded base64, a body that is not UTF-8, a body of another
/// version, a body with too few lines, an expiry that is not a plain decimal number, and an
/// `Authorization` header beside the grant.
#[test]
fn a_request_whose_grant_is_not_shaped_like_one_is_refused() {
    let (config, _) = server(BUCKET);
    let good = signed(&body_of("cas/a"));
    let twice = verify(&config, &carrying(&[good.as_bytes(), good.as_bytes()]));
    assert_eq!(twice.unwrap_err(), MALFORMED);
    let not_text = verify(&config, &carrying(&[&[0xff]]));
    assert_eq!(not_text.unwrap_err(), MALFORMED);
    let over = "a".repeat(MAX_TOKEN_BYTES + 1);
    assert_eq!(refusal_of_token(&config, &over), TOO_LARGE);
    let at_limit = "a".repeat(MAX_TOKEN_BYTES);
    assert_eq!(refusal_of_token(&config, &at_limit), MALFORMED);
    let tag = good.split_once('.').unwrap().1;
    let body = good.split_once('.').unwrap().0;
    for token in [
        "abc".to_owned(),
        "!!!.AAAA".to_owned(),
        format!("{body}.!!"),
        "Zg==.AAAA".to_owned(),
        format!("{body}.{tag}.{tag}"),
        seal(SECRET, &[0xff, 0xfe, 0xfd]),
        signed(&format!(
            "tierstore-grant-v2\n{ID}\n{BUCKET}\n{EXPIRES}\ncas/a"
        )),
        signed(&format!("{MAGIC}\n{ID}\n{BUCKET}\n{EXPIRES}")),
        signed(&format!("{MAGIC}\n{ID}\n{BUCKET}")),
    ] {
        assert_eq!(refusal_of_token(&config, &token), MALFORMED, "{token}");
    }
    for expires in [
        "+5",
        "05",
        "",
        "x",
        "-1",
        " 5",
        "1.5",
        "99999999999999999999",
    ] {
        let body = format!("{MAGIC}\n{ID}\n{BUCKET}\n{expires}\ncas/a");
        assert_eq!(
            refusal_of_token(&config, &signed(&body)),
            MALFORMED,
            "{expires:?}"
        );
    }
    let mut both = carrying_token(&good);
    both.headers
        .insert("authorization", "AWS4-HMAC-SHA256 x".parse().unwrap());
    assert_eq!(verify(&config, &both).unwrap_err(), TWO_CREDENTIALS);
}

/// Asserts that a token the server cannot trust is refused with one message: a key id the server
/// does not have, another secret, a tag that is empty, short or another body's, and a body edited
/// after it was signed (a name added, the expiry moved out, the bucket changed, one byte of a name
/// changed), whatever the edit would have allowed.
#[test]
fn a_grant_with_an_unknown_key_or_a_bad_signature_is_untrusted() {
    let (config, _) = server(BUCKET);
    let good = signed(&body_of("cas/a"));
    let (body, tag) = good.split_once('.').unwrap();
    let other_id = format!("{MAGIC}\nother\n{BUCKET}\n{EXPIRES}\ncas/a");
    let wrong_secret = seal("another-secret-0123456789", body_of("cas/a").as_bytes());
    let short = URL_SAFE_NO_PAD.encode([0_u8; 16]);
    let edit = |body: String| format!("{}.{tag}", URL_SAFE_NO_PAD.encode(body));
    for token in [
        signed(&other_id),
        wrong_secret,
        format!("{body}."),
        format!("{body}.{short}"),
        edit(body_of("cas/a\ncas/b")),
        edit(format!("{MAGIC}\n{ID}\n{BUCKET}\n{}\ncas/a", EXPIRES + 1)),
        edit(format!("{MAGIC}\n{ID}\nother\n{EXPIRES}\ncas/a")),
        edit(body_of("cas/b")),
    ] {
        assert_eq!(refusal_of_token(&config, &token), UNTRUSTED, "{token}");
    }
    assert!(verify(&config, &carrying_token(&good)).is_ok());
}

/// Asserts that a body signed by the right key is still refused when its names are not a list a
/// grant has: unsorted, repeated, empty, with a name the proxy keeps or a request cannot carry, or
/// more than the limit. Such a token is not made by `mint_grant`.
#[test]
fn a_signed_grant_with_a_bad_list_of_names_is_malformed() {
    let (config, _) = server(BUCKET);
    let many: Vec<String> = (0..=MAX_GRANT_KEYS).map(|i| format!("k{i:05}")).collect();
    for names in [
        "cas/b\ncas/a".to_owned(),
        "cas/a\ncas/a".to_owned(),
        "cas/a\n".to_owned(),
        String::new(),
        "cas/a\n_journal/x".to_owned(),
        "cas/a\ncas/../b".to_owned(),
        "cas/a\nb\tc".to_owned(),
        many.join("\n"),
    ] {
        let token = signed(&body_of(&names));
        let shown: String = names.chars().take(20).collect();
        assert_eq!(refusal_of_token(&config, &token), MALFORMED, "{shown:?}");
    }
    let last = many[1..].join("\n");
    assert!(verify(&config, &carrying_token(&signed(&body_of(&last)))).is_ok());
}

/// Asserts the bucket and the clock: a grant for another bucket is refused; one is good until the
/// second it names, which is the first it is refused in, and for good after; an expiry of zero
/// is long past, and the largest expiry does not wrap.
#[test]
fn a_grant_is_for_one_bucket_and_ends_at_its_second() {
    let (config, clock) = server(BUCKET);
    let key = AccessKey::new(ID, SECRET);
    let token = mint_grant(&key, BUCKET, EXPIRES, ["cas/a"]).unwrap();
    let ok = |token: &str| verify(&config, &carrying_token(token));
    assert!(ok(&token).is_ok());
    clock.set(EXPIRES * 1000 - 1);
    assert!(ok(&token).is_ok());
    for now in [
        EXPIRES * 1000,
        EXPIRES * 1000 + 1,
        EXPIRES * 1000 + 3_600_000,
    ] {
        clock.set(now);
        assert_eq!(ok(&token).unwrap_err(), EXPIRED, "{now}");
    }
    let never = mint_grant(&key, BUCKET, u64::MAX, ["cas/a"]).unwrap();
    assert!(ok(&never).is_ok());
    let zero = mint_grant(&key, BUCKET, 0, ["cas/a"]).unwrap();
    assert_eq!(ok(&zero).unwrap_err(), EXPIRED);
    clock.set(NOW);
    let elsewhere = mint_grant(&key, "other-bucket", EXPIRES, ["cas/a"]).unwrap();
    assert_eq!(ok(&elsewhere).unwrap_err(), OTHER_BUCKET);
}

/// Asserts the check of a call against the names of a grant: an object it lists is allowed, one
/// it does not list, a prefix or an extension of a listed name, the bucket, the root and a call
/// with a query parameter but `x-id` (a multipart call, a listing) are each refused.
#[test]
fn a_grant_allows_the_objects_it_lists_and_nothing_else() {
    let names = vec!["cas/b/pack-a".to_owned(), "cas/l/blob-1".to_owned()];
    let call = |target: Target, query: &str| {
        let query = Query::parse(Some(query).filter(|text| !text.is_empty())).unwrap();
        permit(&names, &target, &query)
    };
    let object = |key: &str| Target::Object(key.to_owned());
    for key in ["cas/b/pack-a", "cas/l/blob-1"] {
        assert_eq!(call(object(key), ""), Ok(()), "{key}");
        assert_eq!(call(object(key), "x-id=GetObject"), Ok(()), "{key}");
    }
    for key in [
        "cas/b/pack-b",
        "cas/b/pack",
        "cas/b/pack-a/",
        "cas/b/pack-a2",
        "cas/b/",
        "",
        "CAS/B/PACK-A",
        "cas/l/blob-2",
    ] {
        assert_eq!(call(object(key), ""), Err(NOT_LISTED), "{key:?}");
    }
    for query in [
        "uploads",
        "uploadId=u",
        "partNumber=1",
        "prefix=cas%2F",
        "versionId=1",
        "list-type=2",
        "x-id=GetObject&uploadId=u",
    ] {
        assert_eq!(
            call(object("cas/b/pack-a"), query),
            Err(NOT_ALLOWED),
            "{query}"
        );
    }
    assert_eq!(call(Target::Bucket, "list-type=2"), Err(NOT_ALLOWED));
    assert_eq!(call(Target::Bucket, ""), Err(NOT_ALLOWED));
    assert_eq!(call(Target::Root, ""), Err(NOT_ALLOWED));
}

/// Asserts that the role of a grant does GET and HEAD and no other method, and may not list the
/// open uploads.
#[test]
fn the_role_of_a_grant_reads_and_does_not_list_uploads() {
    let role = Role::Grant(Arc::new(vec!["a".to_owned()]));
    assert_eq!(role.permit(&hyper::Method::GET), Ok(()));
    assert_eq!(role.permit(&hyper::Method::HEAD), Ok(()));
    for method in [
        hyper::Method::PUT,
        hyper::Method::POST,
        hyper::Method::DELETE,
        hyper::Method::PATCH,
        hyper::Method::OPTIONS,
    ] {
        assert_eq!(role.permit(&method), Err(NOT_ALLOWED), "{method}");
    }
    assert_eq!(role.permit_list_uploads(""), Err(NOT_ALLOWED));
    assert_eq!(role.permit_list_uploads("cas/"), Err(NOT_ALLOWED));
}

fn file(dir: &TestDir, name: &str, text: &str, mode: u32) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, Permissions::from_mode(mode)).unwrap();
    path
}

fn json(id: &str, secret: &str) -> String {
    format!("{{\"access_key_id\": \"{id}\", \"secret_access_key\": \"{secret}\"}}")
}

/// Asserts what a grant key file holds: an id and a secret, as JSON of exactly those two fields or
/// as two lines, owned by the user and private; that a file with another shape, a group-readable
/// mode or a link is refused with the flag in the message and never the secret; and that the keys
/// are read in the order of the flags.
#[test]
fn a_grant_key_file_is_a_private_file_with_an_id_and_a_secret() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-grant-keys");
    let as_json = file(&dir, "json", &json(ID, SECRET), 0o600);
    let lines = file(&dir, "lines", &format!("second\n{SECRET}\n"), 0o400);
    let read = grant_keys(&[as_json.clone(), lines]).unwrap();
    assert_eq!(
        read,
        [AccessKey::new(ID, SECRET), AccessKey::new("second", SECRET)]
    );
    assert!(grant_keys(&[]).unwrap().is_empty());
    let bad = [
        (
            "extra",
            json(ID, SECRET).replace('}', ", \"prefix\": \"a/\"}"),
        ),
        ("empty", json(ID, "")),
        ("one", format!("{ID}\n")),
        (
            "kind",
            json(ID, SECRET).replace(&format!("\"{SECRET}\""), "7"),
        ),
    ];
    for (name, text) in bad {
        let path = file(&dir, name, &text, 0o600);
        let refused = grant_keys(&[path]).unwrap_err();
        assert!(refused.starts_with(FLAG_TEXT), "{name}: {refused}");
        assert!(!refused.contains(SECRET), "{name}: {refused}");
    }
    let open = file(&dir, "open", &json(ID, SECRET), 0o644);
    let refused = grant_keys(&[open]).unwrap_err();
    assert!(
        refused.starts_with(FLAG_TEXT) && refused.contains("0600"),
        "{refused}"
    );
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&as_json, &link).unwrap();
    let refused = grant_keys(&[link]).unwrap_err();
    assert!(refused.contains("symbolic link"), "{refused}");
    let missing = dir.path().join("missing");
    assert!(grant_keys(&[missing]).unwrap_err().starts_with(FLAG_TEXT));
}

/// Asserts that the id of a grant key must differ from the ids of the other keys and of the other
/// grant keys, and the message names the id and not the secret.
#[test]
fn grant_key_ids_must_differ_from_each_other_and_from_the_other_keys() {
    let one = AccessKey::new(ID, SECRET);
    let two = AccessKey::new("second", SECRET);
    let keys = [one.clone(), two];
    assert_eq!(ids(&keys), [ID, "second"]);
    assert_eq!(ensure_distinct(&["FULL"], &keys), Ok(()));
    let twice = ensure_distinct(&[], &[one.clone(), one]).unwrap_err();
    assert!(
        twice.contains("\"build-grant\"") && !twice.contains(SECRET),
        "{twice}"
    );
    assert!(ensure_distinct(&[ID], &keys).is_err());
}

/// Asserts that the example token in the README of the packaging is what `mint_grant` makes from
/// the key, the bucket, the expiry and the names the README gives, and that the server check
/// accepts it with those names and refuses it from the second it ends: another component can make
/// the token from the README alone.
#[test]
fn the_token_in_the_readme_is_the_one_the_documented_inputs_make() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let readme = std::fs::read_to_string(root.join("../../packaging/tierstore/README.md")).unwrap();
    let line = readme
        .lines()
        .find(|line| line.starts_with("x-tierstore-grant: "));
    let documented = line
        .expect("the example header")
        .trim_start_matches("x-tierstore-grant: ");
    let key = AccessKey::new("build-grant", "grant-secret-0123456789");
    let names = ["cas/l/blob-1", "cas/b/lease-7/pack-a"];
    let made = mint_grant(&key, "tierstore", 1_800_000_600, names).unwrap();
    assert_eq!(made, documented);
    let (mut config, clock) = server("tierstore");
    config.grant_keys = vec![key];
    let accepted = verify(&config, &carrying_token(documented)).unwrap();
    assert_eq!(
        names_of(&accepted),
        ["cas/b/lease-7/pack-a", "cas/l/blob-1"]
    );
    let body = URL_SAFE_NO_PAD
        .decode(documented.split_once('.').unwrap().0)
        .unwrap();
    let body = String::from_utf8(body).unwrap();
    let listed: Vec<&str> = readme
        .split("```text\n")
        .filter_map(|block| block.split_once("\n```").map(|(text, _)| text))
        .filter(|text| text.starts_with(MAGIC))
        .collect();
    assert_eq!(listed, [body.as_str()]);
    clock.set(1_800_000_600_000);
    assert_eq!(
        verify(&config, &carrying_token(documented)).unwrap_err(),
        EXPIRED
    );
}
