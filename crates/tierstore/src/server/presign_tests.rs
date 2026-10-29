//! The check of presigned URLs, the URLs the library makes, and the presigning key files.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use hyper::Request;
use kbf_testkit::dir::TestDir;

use crate::TestClock;

use super::super::auth::{MALFORMED, MISMATCH, SKEWED, UNKNOWN_KEY, UNSIGNED};
use super::{
    AccessKey, EXPIRED, Grant, PresignError, PresignKey, PresignRequest, TOO_LONG, check_name,
    ensure_distinct, presign_keys, presign_put, presign_url, validate_prefix, verify_query,
};
use super::{Parts, Role, ServerConfig};

/// The example key and secret of the SigV4 documentation.
const DOC_ID: &str = "AKIAIOSFODNN7EXAMPLE";
const DOC_SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

/// 2013-05-24 00:00:00 UTC, the date of the documented example.
const DOC_NOW: u64 = 1_369_353_600_000;

/// The time of the other tests: 2027-01-15 08:00:00 UTC.
pub(crate) const NOW: u64 = 1_800_000_000_000;

pub(crate) const SECRET: &str = "presign-secret-0123456789";

pub(crate) fn grant() -> Grant {
    Grant {
        prefix: "cas/b/".to_owned(),
        max_expires_secs: 600,
        max_object_bytes: 1 << 20,
    }
}

pub(crate) fn key(id: &str) -> PresignKey {
    PresignKey::new(id, SECRET, grant()).unwrap()
}

pub(crate) fn access(id: &str) -> AccessKey {
    AccessKey::new(id, SECRET)
}

/// A server that holds `keys`, at the time `now`, with an hour of `--presign-max-expires`.
fn config(keys: Vec<PresignKey>, now: u64) -> (ServerConfig, Arc<TestClock>) {
    let clock = Arc::new(TestClock::new());
    clock.set(now);
    let mut config = ServerConfig::new("tier-bucket", "FULL", "full-secret", clock.clone());
    config.presign_keys = keys;
    config.presign_max_expires = Duration::from_secs(3600);
    (config, clock)
}

/// The parts of a request for a URL.
pub(crate) fn parts_of(method: &str, url: &str) -> Parts {
    let rest = url.split_once("://").unwrap().1;
    let at = rest.find('/').unwrap();
    let request = Request::builder()
        .method(method)
        .uri(&rest[at..])
        .header("host", &rest[..at])
        .body(())
        .unwrap();
    request.into_parts().0
}

pub(crate) fn request<'a>(key: &'a str, expires_secs: u64) -> PresignRequest<'a> {
    PresignRequest {
        endpoint: "http://tier.test",
        region: "us-east-1",
        bucket: "tier-bucket",
        key,
        expires_secs,
        now: NOW,
    }
}

/// Asserts that the example URL of the SigV4 documentation for query-string authentication
/// (a GET of `/test.txt`, good for a day from 2013-05-24, signed by the example key) passes the
/// check: the canonical request, the query without the signature, `UNSIGNED-PAYLOAD` and the
/// derivation of the key are the documented ones, apart from the code that makes URLs. The
/// signature is the one the documentation gives.
#[test]
fn the_documented_example_url_verifies() {
    let signature = "aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404";
    let url = format!(
        "https://examplebucket.s3.amazonaws.com/test.txt?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request&X-Amz-Date=20130524T000000Z&X-Amz-Expires=86400&X-Amz-SignedHeaders=host&X-Amz-Signature={signature}"
    );
    let held = PresignKey::new(
        DOC_ID,
        DOC_SECRET,
        Grant {
            max_expires_secs: 86_400,
            ..grant()
        },
    )
    .unwrap();
    let (mut config, _) = config(vec![held], DOC_NOW + 1000);
    config.presign_max_expires = Duration::from_secs(86_400);
    let signed = verify_query(&config, &parts_of("GET", &url)).unwrap();
    assert_eq!(signed.role, Role::PresignPut);
    let changed = url.replace("aeeed9bb", "aeeed9bc");
    let refused = verify_query(&config, &parts_of("GET", &changed));
    assert_eq!(refused.unwrap_err(), MISMATCH);
    let other_path = url.replace("/test.txt", "/test.txu");
    let refused = verify_query(&config, &parts_of("GET", &other_path));
    assert_eq!(refused.unwrap_err(), MISMATCH);
}

/// Asserts that the URLs the library makes are accepted by the check, for keys that need
/// encoding (spaces, `+`, `%`, `~`, non-ASCII, `=`, `&`, `?`, `#`), and carry the six parameters.
/// The check is the one the documented example passes, so this is the library against the spec.
#[test]
fn a_url_the_library_makes_is_accepted_by_the_check() {
    let (config, _) = config(vec![key("pack-presign")], NOW);
    let names = [
        "cas/b/x/y",
        "cas/b/a b",
        "cas/b/a+b",
        "cas/b/100%",
        "cas/b/~tilde",
        "cas/b/é/ü",
        "cas/b/k=v&w=x",
        "cas/b/why?",
        "cas/b/frag#ment",
        "cas/b/%2e%2e",
        "cas/b/(paren)*star'quote!",
    ];
    for name in names {
        let url = presign_put(&access("pack-presign"), &request(name, 600)).unwrap();
        for param in [
            "X-Amz-Algorithm=AWS4-HMAC-SHA256",
            "X-Amz-Credential=pack-presign%2F20270115%2Fus-east-1%2Fs3%2Faws4_request",
            "X-Amz-Date=20270115T080000Z",
            "X-Amz-Expires=600",
            "X-Amz-SignedHeaders=host",
            "X-Amz-Signature=",
        ] {
            assert!(url.contains(param), "{url}");
        }
        assert!(
            url.starts_with("http://tier.test/tier-bucket/cas/b/"),
            "{url}"
        );
        let signed = verify_query(&config, &parts_of("PUT", &url));
        assert!(signed.is_ok(), "{name}: {signed:?}");
    }
}

/// Asserts the clock rules of a URL: good until the millisecond before `X-Amz-Date +
/// X-Amz-Expires`, refused from it on with no grace; a date up to 15 minutes ahead of the clock
/// is good and one a millisecond more is `SKEWED`.
#[test]
fn a_url_expires_at_date_plus_expires_with_no_grace() {
    let (config, clock) = config(vec![key("pack-presign")], NOW);
    let url = presign_put(&access("pack-presign"), &request("cas/b/x", 600)).unwrap();
    let check = |at: u64| {
        clock.set(at);
        verify_query(&config, &parts_of("PUT", &url)).map(|_| ())
    };
    assert_eq!(check(NOW), Ok(()));
    assert_eq!(check(NOW + 599_999), Ok(()));
    assert_eq!(check(NOW + 600_000), Err(EXPIRED));
    assert_eq!(check(NOW + 600_001), Err(EXPIRED));
    assert_eq!(check(NOW + 14 * 60_000), Err(EXPIRED));
    assert_eq!(check(NOW - 15 * 60_000), Ok(()));
    assert_eq!(check(NOW - 15 * 60_000 - 1), Err(SKEWED));
}

/// Asserts that `X-Amz-Expires` is held to the smaller of the ceiling of the key and the server's
/// maximum, and that zero is refused; the URL is signed correctly in each case, so only the limit
/// refuses it.
#[test]
fn the_expiry_is_held_to_the_ceiling_and_to_the_server_maximum() {
    let (mut config, _) = config(vec![key("pack-presign")], NOW);
    let url = |secs: u64| {
        let url = presign_url(
            "PUT",
            &access("pack-presign"),
            &request("cas/b/x", secs),
            &[],
        );
        url.unwrap()
    };
    let check = |config: &ServerConfig, secs: u64| {
        verify_query(config, &parts_of("PUT", &url(secs))).map(|_| ())
    };
    assert_eq!(check(&config, 1), Ok(()));
    assert_eq!(check(&config, 600), Ok(()));
    assert_eq!(check(&config, 601), Err(TOO_LONG));
    assert_eq!(check(&config, 0), Err(TOO_LONG));
    assert_eq!(check(&config, 604_800), Err(TOO_LONG));
    config.presign_max_expires = Duration::from_secs(60);
    assert_eq!(check(&config, 60), Ok(()));
    assert_eq!(check(&config, 61), Err(TOO_LONG));
}

/// Asserts what a changed URL is: a changed signature, key, bucket, method, date, expiry, host,
/// credential or query is `SignatureDoesNotMatch` (or the limit's error for an expiry made
/// longer); a URL signed by a key the server does not hold, or by another secret, is refused.
#[test]
fn a_changed_url_does_not_verify() {
    let (config, _) = config(vec![key("pack-presign")], NOW);
    let url = presign_put(&access("pack-presign"), &request("cas/b/x", 300)).unwrap();
    let signed = |url: &str| verify_query(&config, &parts_of("PUT", url)).map(|_| ());
    assert_eq!(signed(&url), Ok(()));
    let signature = url.rsplit('=').next().unwrap().to_owned();
    let flipped = format!(
        "{}{}",
        &signature[..63],
        if signature.ends_with('0') { '1' } else { '0' }
    );
    let changed = [
        url.replace(&signature, &flipped),
        url.replace("/cas/b/x", "/cas/b/y"),
        url.replace("/tier-bucket/", "/other/"),
        url.replace("X-Amz-Expires=300", "X-Amz-Expires=301"),
        url.replace("T080000Z", "T080001Z"),
        url.replace("http://tier.test", "http://other.test"),
        format!("{url}&extra=1"),
        url.replace(
            "X-Amz-SignedHeaders=host",
            "X-Amz-SignedHeaders=host%3Bx-extra",
        ),
    ];
    for changed in changed {
        let result = signed(&changed);
        assert!(
            [Err(MISMATCH), Err(MALFORMED)].contains(&result),
            "{changed}: {result:?}"
        );
    }
    let get = verify_query(&config, &parts_of("GET", &url));
    assert_eq!(get.unwrap_err(), MISMATCH);
    let delete = verify_query(&config, &parts_of("DELETE", &url));
    assert_eq!(delete.unwrap_err(), MISMATCH);
    let wrong = presign_put(
        &AccessKey::new("pack-presign", "another"),
        &request("cas/b/x", 300),
    );
    assert_eq!(signed(&wrong.unwrap()), Err(MISMATCH));
    let stranger = presign_put(&access("not-held"), &request("cas/b/x", 300)).unwrap();
    assert_eq!(signed(&stranger), Err(UNKNOWN_KEY));
    let west = PresignRequest {
        region: "eu-west-1",
        ..request("cas/b/x", 300)
    };
    let west = presign_put(&access("pack-presign"), &west).unwrap();
    assert_eq!(signed(&west), Err(UNKNOWN_KEY));
}

/// Asserts that a URL that is not well formed is refused whatever it is signed with: no
/// parameter of the six, one twice, another algorithm, a credential scope of other parts, a date
/// that is not one, a date that is not the day of the scope, a non-number for the expiry, and
/// headers that do not include `host`; and that a request with no signature at all is unsigned.
#[test]
fn a_url_that_is_not_well_formed_is_refused() {
    let (config, _) = config(vec![key("pack-presign")], NOW);
    let url = presign_put(&access("pack-presign"), &request("cas/b/x", 300)).unwrap();
    let signed = |url: &str| verify_query(&config, &parts_of("PUT", url)).map(|_| ());
    for name in [
        "X-Amz-Algorithm",
        "X-Amz-Credential",
        "X-Amz-Date",
        "X-Amz-Expires",
        "X-Amz-SignedHeaders",
        "X-Amz-Signature",
    ] {
        let without: Vec<&str> = url
            .split(['?', '&'])
            .skip(1)
            .filter(|pair| !pair.starts_with(name))
            .collect();
        let base = url.split('?').next().unwrap();
        let removed = format!("{base}?{}", without.join("&"));
        let want = if name == "X-Amz-Algorithm" {
            UNSIGNED
        } else {
            MALFORMED
        };
        assert_eq!(signed(&removed), Err(want), "{name}");
        let twice = format!("{url}&{name}=x");
        assert_eq!(signed(&twice), Err(MALFORMED), "{name}");
    }
    let broken = [
        url.replace("AWS4-HMAC-SHA256", "AWS4-HMAC-SHA512"),
        url.replace("%2Fs3%2F", "%2Fsts%2F"),
        url.replace("%2Faws4_request", "%2Faws4_request%2Fmore"),
        url.replace("pack-presign%2F", "pack-presign%2F%2F"),
        url.replace("20270115T080000Z", "20270230T080000Z"),
        url.replace("20270115T080000Z", "20270116T080000Z"),
        url.replace("X-Amz-Expires=300", "X-Amz-Expires=soon"),
        url.replace("X-Amz-Expires=300", "X-Amz-Expires=-1"),
        url.replace(
            "pack-presign%2F20270115%2Fus-east-1%2Fs3%2Faws4_request",
            "pack-presign",
        ),
        url.replace("%2Fus-east-1%2Fs3%2Faws4_request", ""),
        url.replace(
            "pack-presign%2F20270115%2Fus-east-1%2Fs3%2Faws4_request",
            "pack-presign%2F20270115",
        ),
        format!("{url}&bad=%FF"),
    ];
    for broken in broken {
        assert_eq!(signed(&broken), Err(MALFORMED), "{broken}");
    }
    let signature = url.rsplit('=').next().unwrap().to_owned();
    for not_hex in ["zz", "", "abc"] {
        let result = signed(&url.replace(&signature, not_hex));
        assert_eq!(result, Err(MISMATCH), "{not_hex:?}");
    }
    let no_host = url.replace("X-Amz-SignedHeaders=host", "X-Amz-SignedHeaders=range");
    let result = signed(&no_host);
    assert!(result.is_err(), "{result:?}");
    let plain = verify_query(
        &config,
        &parts_of("PUT", "http://tier.test/tier-bucket/cas/b/x"),
    );
    assert_eq!(plain.unwrap_err(), UNSIGNED);
}

/// Asserts that a URL signed with a key that is not a presigning key of the server is refused, the
/// full key's included: a URL is never signed with the rights of the full key.
#[test]
fn the_full_key_cannot_sign_a_url() {
    let (config, _) = config(vec![key("pack-presign")], NOW);
    let full = AccessKey::new("FULL", "full-secret");
    let url = presign_put(&full, &request("cas/b/x", 300)).unwrap();
    let refused = verify_query(&config, &parts_of("PUT", &url));
    assert_eq!(refused.unwrap_err(), UNKNOWN_KEY);
}

/// Asserts the names a key or a prefix may be, and the prefixes a key may have.
#[test]
fn names_and_prefixes_are_checked() {
    for good in ["cas/b/a", "a.b", "a/.b", "a/b./c", "é/ü", "x/%2e"] {
        assert_eq!(check_name(good), Ok(()), "{good}");
    }
    for bad in [
        "a/../b", "..", "a\0b", "a/./b", "./a", "a/.", "a\u{7f}", "a\tb",
    ] {
        assert!(check_name(bad).is_err(), "{bad:?}");
    }
    assert!(check_name(&"a".repeat(1025)).is_err());
    assert_eq!(check_name(&"a".repeat(1024)), Ok(()));
    assert_eq!(validate_prefix("cas/b/"), Ok(()));
    for bad in [
        "",
        "_",
        "_jou",
        "_journal/",
        "_journal/p1/",
        "_tiering/usage",
        "a/../",
    ] {
        assert!(validate_prefix(bad).is_err(), "{bad:?}");
    }
}

/// Asserts that the library refuses what it should not sign: a key that is not a valid name or is
/// empty, an expiry of zero or over 7 days, and an endpoint that is not `http(s)://authority`;
/// and that the limits themselves are made.
#[test]
fn the_library_refuses_to_sign_what_a_server_would_refuse() {
    let make = |key: &str, secs: u64, endpoint: &str| {
        let wanted = PresignRequest {
            endpoint,
            ..request(key, secs)
        };
        presign_put(&access("k"), &wanted)
    };
    assert!(make("a/../b", 60, "http://h").is_err());
    assert!(make("a\0", 60, "http://h").is_err());
    assert_eq!(make("", 60, "http://h"), Err(PresignError::Name("empty")));
    assert_eq!(make("a", 0, "http://h"), Err(PresignError::Expires));
    assert_eq!(make("a", 604_801, "http://h"), Err(PresignError::Expires));
    for endpoint in ["h", "ftp://h", "http://", "http://h/x", ""] {
        assert_eq!(
            make("a", 60, endpoint),
            Err(PresignError::Endpoint),
            "{endpoint}"
        );
    }
    assert!(make("a", 1, "https://h:9000").is_ok());
    assert!(make("a", 604_800, "http://h").is_ok());
    let url = make("a", 60, "https://h:9000").unwrap();
    assert!(url.starts_with("https://h:9000/tier-bucket/a?"), "{url}");
}

/// Asserts the rules of a presigning key and its grant.
#[test]
fn a_presigning_key_needs_a_grant_that_can_be_used() {
    let bad = [
        Grant {
            prefix: String::new(),
            ..grant()
        },
        Grant {
            prefix: "_tiering/".to_owned(),
            ..grant()
        },
        Grant {
            max_expires_secs: 0,
            ..grant()
        },
        Grant {
            max_expires_secs: 604_801,
            ..grant()
        },
        Grant {
            max_object_bytes: 0,
            ..grant()
        },
        Grant {
            max_object_bytes: (128 << 20) + 1,
            ..grant()
        },
    ];
    for grant in bad {
        assert!(PresignKey::new("k", SECRET, grant).is_err());
    }
    for (id, secret) in [
        ("", "s"),
        ("k", ""),
        ("a/b", "s"),
        ("a,b", "s"),
        ("a b", "s"),
        ("é", "s"),
    ] {
        assert!(PresignKey::new(id, secret, grant()).is_err(), "{id:?}");
    }
    let edge = Grant {
        max_expires_secs: 604_800,
        max_object_bytes: 128 << 20,
        ..grant()
    };
    assert!(PresignKey::new("k", SECRET, edge).is_ok());
    let shown = format!("{:?}", key("pack-presign"));
    assert!(
        shown.contains("pack-presign") && !shown.contains(SECRET),
        "{shown}"
    );
}

/// A file in `dir` with this text and mode.
fn file(dir: &TestDir, name: &str, text: &str, mode: u32) -> std::path::PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, Permissions::from_mode(mode)).unwrap();
    path
}

/// Asserts what a presigning key file holds: the key and its ceiling; and that a file without a
/// ceiling, in the two-line form, with a field more, a field missing, an unusable ceiling or a
/// group-readable mode is refused, with no message that carries the secret (fail closed).
#[test]
fn a_presigning_key_file_has_a_ceiling_and_is_private() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-presign-keys");
    let json = |ceiling: &str| {
        format!(
            "{{\"access_key_id\": \"pack-presign\", \"secret_access_key\": \"{SECRET}\"{ceiling}}}"
        )
    };
    let full = ", \"ceiling\": {\"prefix\": \"cas/b/\", \"max_expires_secs\": 600, \"max_object_bytes\": 1048576}";
    let path = file(&dir, "good", &json(full), 0o600);
    let read = PresignKey::from_file("--presign-key-file", &path).unwrap();
    assert_eq!(read.id(), "pack-presign");
    assert_eq!(read.grant(), &grant());
    let bad = [
        ("none", json("")),
        ("lines", format!("pack-presign\n{SECRET}\n")),
        (
            "extra",
            json(&full.replace("\"prefix\"", "\"x\": 1, \"prefix\"")),
        ),
        (
            "missing",
            json(&full.replace("\"max_object_bytes\": 1048576", "\"x\": 1")),
        ),
        (
            "nosize",
            json(&full.replace(", \"max_object_bytes\": 1048576", "")),
        ),
        ("prefix", json(&full.replace("cas/b/", "_journal/"))),
        ("ttl", json(&full.replace("600", "0"))),
        ("big", json(&full.replace("1048576", "999999999999"))),
        ("type", json(&full.replace("600", "\"600\""))),
    ];
    for (name, text) in bad {
        let path = file(&dir, name, &text, 0o600);
        let refused = PresignKey::from_file("--presign-key-file", &path).unwrap_err();
        assert!(
            refused.starts_with("--presign-key-file "),
            "{name}: {refused}"
        );
        assert!(!refused.contains(SECRET), "{name}: {refused}");
    }
    let open = file(&dir, "open", &json(full), 0o644);
    let refused = PresignKey::from_file("--presign-key-file", &open).unwrap_err();
    assert!(refused.contains("0600"), "{refused}");
    let missing = dir.path().join("missing");
    assert!(PresignKey::from_file("--presign-key-file", &missing).is_err());
}

/// Asserts that the flag reads its files in order and that a key id used twice, or the id of
/// another key, is refused without a secret in the message.
#[test]
fn presigning_key_ids_must_differ_from_each_other_and_from_the_other_keys() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-presign-ids");
    let text = |id: &str| {
        format!(
            "{{\"access_key_id\": \"{id}\", \"secret_access_key\": \"{SECRET}\", \"ceiling\": {{\"prefix\": \"cas/b/\", \"max_expires_secs\": 60, \"max_object_bytes\": 10}}}}"
        )
    };
    let one = file(&dir, "one", &text("one"), 0o600);
    let two = file(&dir, "two", &text("two"), 0o600);
    let keys = presign_keys(&[one.clone(), two.clone()]).unwrap();
    assert_eq!(
        keys.iter().map(PresignKey::id).collect::<Vec<_>>(),
        ["one", "two"]
    );
    assert_eq!(ensure_distinct(&["FULL", "READER"], &keys), Ok(()));
    let again = presign_keys(&[one.clone(), two, one]).unwrap();
    let refused = ensure_distinct(&[], &again).unwrap_err();
    assert!(
        refused.contains("\"one\"") && !refused.contains(SECRET),
        "{refused}"
    );
    let clash = ensure_distinct(&["FULL", "two"], &keys).unwrap_err();
    assert!(clash.contains("\"two\""), "{clash}");
}
