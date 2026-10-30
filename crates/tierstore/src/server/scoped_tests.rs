//! The scoped key files and the checks of a scoped key's calls.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use hyper::Method;
use kbf_testkit::dir::TestDir;

use super::keys::Role;
use super::scoped::{
    NOT_ALLOWED, OUT_OF_SCOPE, Prefixes, ScopedKey, ensure_distinct, ids, scoped_keys,
};

const SECRET: &str = "scoped-secret-0123456789";
const FLAG: &str = "--scoped-key-file";

fn file(dir: &TestDir, name: &str, text: &str, mode: u32) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, Permissions::from_mode(mode)).unwrap();
    path
}

/// The JSON of a key file with these three lists (each the text of a JSON array).
fn json(read: &str, write: &str, delete: &str) -> String {
    format!(
        "{{\"access_key_id\": \"packs\", \"secret_access_key\": \"{SECRET}\", \
         \"read_prefixes\": {read}, \"write_prefixes\": {write}, \"delete_prefixes\": {delete}}}"
    )
}

/// Asserts what a scoped key file holds: the id, the secret and three lists of prefixes that
/// end in `/`, read back as written, overlapping prefixes and empty lists included.
#[test]
fn a_scoped_key_file_holds_three_lists_of_prefixes() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-scoped-keys");
    let good = file(
        &dir,
        "good",
        &json(r#"["cas/", "cas/f/"]"#, r#"["cas/f/fe1/"]"#, "[]"),
        0o600,
    );
    let read = ScopedKey::from_file(FLAG, &good).unwrap();
    assert_eq!(read.id(), "packs");
    assert_eq!(read.read_prefixes(), ["cas/", "cas/f/"]);
    assert_eq!(read.write_prefixes(), ["cas/f/fe1/"]);
    assert!(read.delete_prefixes().is_empty());
    let one = file(&dir, "one", &json("[]", "[]", r#"["d/"]"#), 0o600);
    assert!(ScopedKey::from_file(FLAG, &one).is_ok());
    let shown = format!("{read:?}");
    assert!(
        shown.contains("packs") && shown.contains("cas/f/fe1/") && !shown.contains(SECRET),
        "{shown}"
    );
}

/// Asserts that a key file is refused, with a message led by the flag that carries no secret,
/// when: all three lists are empty; a prefix is empty, lacks its closing slash, starts with a
/// slash, has `..` or a `.` segment, is `_journal/` or `_tiering/`, is a prefix of one of them,
/// is under one, or has one as a segment; the same in each of the three lists; a field is
/// missing, unknown or of another type; the file is the two-line form, is group-readable or is a
/// link.
#[test]
fn a_scoped_key_file_that_is_not_valid_is_refused() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-scoped-bad");
    let bad_prefixes = [
        "",
        "cas",
        "/cas/",
        "cas/../",
        "../cas/",
        "cas/./x/",
        "_journal/",
        "_tiering/",
        "_journal/p1/",
        "_tiering/x/",
        "cas/_journal/",
        "cas/_tiering/b/",
    ];
    let mut cases = vec![
        ("empty", json("[]", "[]", "[]")),
        (
            "missing",
            format!("{{\"access_key_id\": \"k\", \"secret_access_key\": \"{SECRET}\"}}"),
        ),
        (
            "nodelete",
            format!(
                "{{\"access_key_id\": \"k\", \"secret_access_key\": \"{SECRET}\", \
                 \"read_prefixes\": [\"a/\"], \"write_prefixes\": []}}"
            ),
        ),
        (
            "noread",
            format!(
                "{{\"access_key_id\": \"k\", \"secret_access_key\": \"{SECRET}\", \
                 \"write_prefixes\": [\"w/\"], \"delete_prefixes\": [\"d/\"]}}"
            ),
        ),
        (
            "nowrite",
            format!(
                "{{\"access_key_id\": \"k\", \"secret_access_key\": \"{SECRET}\", \
                 \"read_prefixes\": [\"r/\"], \"delete_prefixes\": [\"d/\"]}}"
            ),
        ),
        (
            "noid",
            format!(
                "{{\"secret_access_key\": \"{SECRET}\", \"read_prefixes\": [\"r/\"], \
                 \"write_prefixes\": [], \"delete_prefixes\": []}}"
            ),
        ),
        (
            "nosecret",
            "{\"access_key_id\": \"k\", \"read_prefixes\": [\"r/\"], \
             \"write_prefixes\": [], \"delete_prefixes\": []}"
                .to_owned(),
        ),
        (
            "extra",
            json(r#"["a/"]"#, "[]", "[]").replace('}', ", \"ops\": []}"),
        ),
        ("type", json(r#""a/""#, "[]", "[]")),
        ("item", json("[7]", "[]", "[]")),
        ("lines", format!("packs\n{SECRET}\n")),
    ];
    for prefix in bad_prefixes {
        let list = format!("[\"a/\", \"{prefix}\"]");
        cases.push(("read", json(&list, "[]", "[]")));
        cases.push(("write", json("[]", &list, "[]")));
        cases.push(("delete", json("[]", "[]", &list)));
    }
    for (n, (name, text)) in cases.into_iter().enumerate() {
        let path = file(&dir, &format!("{name}{n}"), &text, 0o600);
        let refused = ScopedKey::from_file(FLAG, &path).unwrap_err();
        assert!(
            refused.starts_with("--scoped-key-file "),
            "{name}: {refused}"
        );
        assert!(!refused.contains(SECRET), "{name}: {refused}");
    }
    let open = file(&dir, "open", &json(r#"["a/"]"#, "[]", "[]"), 0o644);
    assert!(
        ScopedKey::from_file(FLAG, &open)
            .unwrap_err()
            .contains("0600")
    );
    let good = file(&dir, "good", &json(r#"["a/"]"#, "[]", "[]"), 0o600);
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&good, &link).unwrap();
    let refused = ScopedKey::from_file(FLAG, &link).unwrap_err();
    assert!(refused.contains("symbolic link"), "{refused}");
}

/// Asserts that a key made in code is refused as a file is: an empty id, an empty secret and an id
/// with a `/` share one reason; three empty lists, a prefix with no closing slash and a reserved
/// prefix each have their own; and a prefix that only looks like a reserved name is made.
#[test]
fn a_scoped_key_made_in_code_is_checked_as_a_file_is() {
    let made = |id: &str, secret: &str, read: &[&str]| {
        let prefixes = Prefixes {
            read,
            ..Prefixes::default()
        };
        ScopedKey::new(id, secret, &prefixes).map(|_| ())
    };
    assert!(made("a", "s", &["x/"]).is_ok());
    assert!(made("a", "s", &["_journal_x/", "journal/"]).is_ok());
    let reasons = [
        made("", "s", &["x/"]),
        made("a", "", &["x/"]),
        made("a/b", "s", &["x/"]),
        made("a", "s", &[]),
        made("a", "s", &["x"]),
        made("a", "s", &["x/_journal/"]),
    ]
    .map(|made| made.unwrap_err().to_string());
    assert_eq!(reasons[0], reasons[1], "{reasons:?}");
    assert_eq!(reasons[0], reasons[2], "{reasons:?}");
    assert!(reasons[3].contains("all empty"), "{reasons:?}");
    assert!(reasons[4].contains("ending in /"), "{reasons:?}");
    assert!(reasons[5].contains("_journal/"), "{reasons:?}");
    assert_ne!(reasons[0], reasons[3], "{reasons:?}");
}

/// Asserts that the ids of the scoped keys must differ from each other and from the other keys,
/// and that the files are read in the order of the flags.
#[test]
fn scoped_key_ids_must_differ_from_each_other_and_from_the_other_keys() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-scoped-ids");
    let one = file(&dir, "one", &json(r#"["a/"]"#, "[]", "[]"), 0o600);
    let keys = scoped_keys(std::slice::from_ref(&one)).unwrap();
    assert_eq!(ensure_distinct(&["FULL"], &keys), Ok(()));
    assert_eq!(ids(&keys), ["packs"]);
    let again = ensure_distinct(&["packs"], &keys).unwrap_err();
    assert!(
        again.contains("\"packs\"") && !again.contains(SECRET),
        "{again}"
    );
    let twice = scoped_keys(&[one.clone(), one]).unwrap();
    assert!(
        ensure_distinct(&[], &twice)
            .unwrap_err()
            .contains("used twice")
    );
    let missing = dir.path().join("missing");
    let refused = scoped_keys(&[missing]).unwrap_err();
    assert!(refused.starts_with("--scoped-key-file "), "{refused}");
}

/// Asserts what the role of a scoped key permits before the path is known: the five methods of
/// the S3 calls it may make, and no other; and that a listing of uploads is held to the read
/// prefixes.
#[test]
fn the_scoped_role_allows_five_methods_and_lists_uploads_under_read_prefixes() {
    let key = ScopedKey::new(
        "a",
        "s",
        &Prefixes {
            read: &["r/"],
            write: &["w/"],
            delete: &[],
        },
    )
    .unwrap();
    let config = super::ServerConfig {
        scoped_keys: vec![key],
        ..super::ServerConfig::new("b", "k", "s", Arc::new(crate::TestClock::new()))
    };
    let (secret, role) = config.key("a").expect("the scoped key is found");
    assert_eq!(secret, "s");
    assert!(matches!(role, Role::Scoped(_)));
    for method in [
        Method::GET,
        Method::HEAD,
        Method::PUT,
        Method::POST,
        Method::DELETE,
    ] {
        assert_eq!(role.permit(&method), Ok(()), "{method}");
    }
    for method in [
        Method::PATCH,
        Method::OPTIONS,
        Method::TRACE,
        Method::CONNECT,
    ] {
        assert_eq!(role.permit(&method), Err(NOT_ALLOWED), "{method}");
    }
    assert_eq!(role.permit_list_uploads("r/x"), Ok(()));
    for outside in ["", "r", "w/", "other/"] {
        assert_eq!(
            role.permit_list_uploads(outside),
            Err(OUT_OF_SCOPE),
            "{outside:?}"
        );
    }
    assert!(config.key("b").is_none());
}
