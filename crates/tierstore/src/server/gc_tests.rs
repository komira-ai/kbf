//! The GC key files and the checks of a GC key's calls.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use hyper::Request;
use kbf_testkit::dir::TestDir;

use super::gc::{GcKey, NOT_ALLOWED, OUT_OF_SCOPE, ensure_distinct, gc_keys, permit};
use super::presign::PresignError;
use super::request::{Query, Target};

const SECRET: &str = "gc-secret-0123456789";
const FLAG: &str = "--gc-key-file";

fn file(dir: &TestDir, name: &str, text: &str, mode: u32) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, Permissions::from_mode(mode)).unwrap();
    path
}

fn json(prefix: &str) -> String {
    format!(
        "{{\"access_key_id\": \"pack-gc\", \"secret_access_key\": \"{SECRET}\", \"prefix\": \"{prefix}\"}}"
    )
}

/// Asserts what a GC key file holds: the id, the secret and a prefix that ends in `/`, nothing
/// else; and that a file with another prefix shape (none, no closing slash, reserved), a field
/// more or less, the two-line form, a link, or a group-readable mode is refused, with no message
/// that carries the secret.
#[test]
fn a_gc_key_file_has_a_prefix_ending_in_a_slash_and_is_private() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-gc-keys");
    let good = file(&dir, "good", &json("cas/b/"), 0o600);
    let read = GcKey::from_file(FLAG, &good).unwrap();
    assert_eq!((read.id(), read.prefix()), ("pack-gc", "cas/b/"));
    let bad = [
        ("noslash", json("cas/b")),
        ("slash", json("/")),
        ("leading", json("/cas/b/")),
        ("empty", json("")),
        ("reserved", json("_journal/")),
        ("dots", json("cas/../")),
        ("extra", json("cas/b/").replace("}", ", \"ops\": []}")),
        (
            "missing",
            format!("{{\"access_key_id\": \"k\", \"secret_access_key\": \"{SECRET}\"}}"),
        ),
        ("lines", format!("pack-gc\n{SECRET}\n")),
        ("type", json("cas/b/").replace("\"cas/b/\"", "7")),
    ];
    for (name, text) in bad {
        let path = file(&dir, name, &text, 0o600);
        let refused = GcKey::from_file(FLAG, &path).unwrap_err();
        assert!(refused.starts_with("--gc-key-file "), "{name}: {refused}");
        assert!(!refused.contains(SECRET), "{name}: {refused}");
    }
    let open = file(&dir, "open", &json("cas/b/"), 0o644);
    assert!(GcKey::from_file(FLAG, &open).unwrap_err().contains("0600"));
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&good, &link).unwrap();
    let refused = GcKey::from_file(FLAG, &link).unwrap_err();
    assert!(refused.contains("symbolic link"), "{refused}");
    let shown = format!("{:?}", GcKey::from_file(FLAG, &good).unwrap());
    assert!(
        shown.contains("pack-gc") && !shown.contains(SECRET),
        "{shown}"
    );
}

/// Asserts that the ids of the GC keys must differ from each other and from the other keys.
#[test]
fn gc_key_ids_must_differ_from_each_other_and_from_the_other_keys() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-gc-ids");
    let one = file(&dir, "one", &json("cas/b/"), 0o600);
    let keys = gc_keys(std::slice::from_ref(&one)).unwrap();
    assert_eq!(ensure_distinct(&["FULL"], &keys), Ok(()));
    let twice = gc_keys(&[one.clone(), one]).unwrap();
    let refused = ensure_distinct(&[], &twice).unwrap_err();
    assert!(
        refused.contains("\"pack-gc\"") && !refused.contains(SECRET),
        "{refused}"
    );
    assert!(ensure_distinct(&["pack-gc"], &keys).is_err());
}

/// Asserts the arms of the check of a call directly: the methods and targets a GC key may use and
/// the ones it may not, with the one refusal for a call of another kind.
#[test]
fn a_gc_key_may_list_head_delete_and_abort_and_nothing_else() {
    let call = |method: &str, uri: &str| {
        let (parts, ()) = Request::builder()
            .method(method)
            .uri(uri)
            .body(())
            .unwrap()
            .into_parts();
        let query = Query::parse(parts.uri.query()).unwrap();
        let target = match parts.uri.path().strip_prefix("/b/") {
            Some(key) if !key.is_empty() => Target::Object(key.to_owned()),
            _ => Target::Bucket,
        };
        permit("cas/b/", &parts, &target, &query)
    };
    for (method, uri) in [
        ("GET", "/b?list-type=2&prefix=cas%2Fb%2F"),
        ("GET", "/b?list-type=2&prefix=cas%2Fb%2Fx"),
        ("GET", "/b?uploads&prefix=cas%2Fb%2F"),
        ("HEAD", "/b/cas/b/x"),
        ("DELETE", "/b/cas/b/x"),
        ("DELETE", "/b/cas/b/x?uploadId=u"),
    ] {
        assert_eq!(call(method, uri), Ok(()), "{method} {uri}");
    }
    for (method, uri) in [
        ("GET", "/b/cas/b/x"),
        ("GET", "/b?list-type=2"),
        ("GET", "/b?list-type=2&prefix=cas%2F"),
        ("HEAD", "/b"),
        ("HEAD", "/b/cas/b/"),
        ("HEAD", "/b/cas/b/x?uploadId=u"),
        ("DELETE", "/b/cas/c/x"),
        ("DELETE", "/b/cas/b/"),
        ("PUT", "/b/cas/b/x"),
        ("POST", "/b/cas/b/x?uploads"),
        ("PUT", "/b"),
    ] {
        let refused = call(method, uri).unwrap_err();
        assert_eq!(refused.code, "AccessDenied", "{method} {uri}");
    }
    assert_eq!(call("PUT", "/b/cas/b/x"), Err(NOT_ALLOWED));
}

/// Asserts that a key needs an id and a secret that are not empty and an id that a credential
/// scope can carry: an empty id or secret, and an id with `/`, `,`, `=`, a space or a character
/// that is not printable ASCII are each a key error, and a good one is made.
#[test]
fn a_gc_key_needs_a_plain_id_and_a_secret() {
    for (id, secret) in [
        ("", SECRET),
        ("k", ""),
        ("a/b", SECRET),
        ("a,b", SECRET),
        ("a=b", SECRET),
        ("a b", SECRET),
        ("a\tb", SECRET),
        ("é", SECRET),
    ] {
        let made = GcKey::new(id, secret, "cas/b/");
        assert!(
            matches!(made, Err(PresignError::Key(_))),
            "{id:?} {secret:?}"
        );
    }
    assert!(GcKey::new("pack-gc", SECRET, "cas/b/").is_ok());
    for prefix in ["/", "/cas/b/", "cas/b", ""] {
        let made = GcKey::new("pack-gc", SECRET, prefix);
        assert!(matches!(made, Err(PresignError::Key(_))), "{prefix:?}");
    }
}

/// Asserts that a key or a list prefix that starts with the prefix of the key but is not a valid
/// name is refused: a `..` or a `.` segment, a control character, a name over 1024 bytes. The
/// names are given as they are after the path is decoded, so each is a name the check sees.
#[test]
fn a_name_under_the_prefix_that_is_not_a_valid_name_is_refused() {
    let long = format!("cas/b/{}", "x".repeat(1100));
    let bad = [
        "cas/b/x/../y",
        "cas/b/./y",
        "cas/b/y/.",
        "cas/b/y\u{7}",
        "cas/b/y\0",
        long.as_str(),
    ];
    for name in bad {
        let (parts, ()) = Request::builder()
            .method("DELETE")
            .uri("/b/k")
            .body(())
            .unwrap()
            .into_parts();
        let query = Query::parse(None).unwrap();
        let target = Target::Object(name.to_owned());
        let key = permit("cas/b/", &parts, &target, &query);
        assert_eq!(key, Err(OUT_OF_SCOPE), "key {name:?}");
        let (parts, ()) = Request::builder()
            .method("GET")
            .uri("/b")
            .body(())
            .unwrap()
            .into_parts();
        let encoded: String = name.bytes().map(|byte| format!("%{byte:02X}")).collect();
        let uri = format!("/b?list-type=2&prefix={encoded}");
        let query = Query::parse(Some(uri.split_once('?').unwrap().1)).unwrap();
        let listed = permit("cas/b/", &parts, &Target::Bucket, &query);
        assert_eq!(listed, Err(OUT_OF_SCOPE), "list {name:?}");
    }
}

/// Asserts the second guard on the keys of the proxy: the check of a call, given a reserved
/// prefix directly (as a grant built another way than `GcKey::new`, which refuses one, would
/// have), refuses a delete of a key under it and a list of it.
#[test]
fn the_keys_of_the_proxy_are_refused_even_under_a_reserved_prefix() {
    for prefix in ["_journal/", "_tiering/"] {
        let (parts, ()) = Request::builder()
            .method("DELETE")
            .uri("/b/k")
            .body(())
            .unwrap()
            .into_parts();
        let target = Target::Object(format!("{prefix}p1/0001"));
        let query = Query::parse(None).unwrap();
        let deleted = permit(prefix, &parts, &target, &query);
        assert_eq!(deleted, Err(OUT_OF_SCOPE), "delete under {prefix}");
        let (parts, ()) = Request::builder()
            .method("GET")
            .uri("/b")
            .body(())
            .unwrap()
            .into_parts();
        let encoded = prefix.replace('/', "%2F");
        let listing = format!("list-type=2&prefix={encoded}");
        let query = Query::parse(Some(&listing)).unwrap();
        let listed = permit(prefix, &parts, &Target::Bucket, &query);
        assert_eq!(listed, Err(OUT_OF_SCOPE), "list of {prefix}");
    }
}
