//! The access keys: the credentials files, the lookup of a key by its id, and what a role
//! permits.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use hyper::Method;
use kbf_testkit::dir::TestDir;

use crate::TestClock;
use crate::server::ServerConfig;
use crate::server::auth::READ_ONLY;
use crate::server::gc::OUT_OF_SCOPE;
use crate::server::presign::PUT_ONLY;

use super::{AccessKey, Role, ensure_distinct, read_credentials, read_only_keys};

/// A secret that no message may carry.
const SECRET: &str = "topsecret-value-7731";

/// The flags whose files are credentials files.
const FLAGS: [&str; 4] = [
    "--access-key-file",
    "--read-only-key-file",
    "--hot-credentials-file",
    "--cold-credentials-file",
];

/// A file in `dir`, called `name`, with this text and mode.
fn file(dir: &TestDir, name: &str, text: &str, mode: u32) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, Permissions::from_mode(mode)).unwrap();
    path
}

/// The field of the secret in the JSON form.
fn secret_field() -> String {
    format!("\"secret_access_key\": \"{SECRET}\"")
}

/// The JSON form of the key `AKID` and the secret `SECRET`, on one line.
fn json() -> String {
    format!("{{\"access_key_id\": \"AKID\", {}}}", secret_field())
}

/// Asserts that a credentials file holds its key as JSON with the two fields, in either order
/// and with any white space around, or as two lines with either line ending and with white
/// space around, and that the same key is read from every form, for the flag of the full key, of
/// a read-only key and of each tier, with the mode 0600 and with the mode 0400.
#[test]
fn both_forms_of_a_credentials_file_give_the_key() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-keys-forms");
    let want = Ok(("AKID".to_owned(), SECRET.to_owned()));
    let field = secret_field();
    let swapped = format!("{{{field}, \"access_key_id\": \"AKID\"}}");
    let pretty = format!("{{\n  \"access_key_id\": \"AKID\",\n  {field}\n}}\n");
    let forms = [
        ("lines", format!("AKID\n{SECRET}\n"), 0o600),
        ("crlf", format!("AKID\r\n{SECRET}\r\n"), 0o600),
        ("spaced", format!("  AKID \n {SECRET} \n"), 0o600),
        ("lines_0400", format!("AKID\n{SECRET}\n"), 0o400),
        ("json", json(), 0o600),
        ("json_swapped", swapped, 0o600),
        ("json_pretty", pretty, 0o600),
        ("json_padded", format!("\n \t{}\n\n", json()), 0o600),
        ("json_0400", json(), 0o400),
    ];
    for (name, text, mode) in forms {
        let path = file(&dir, name, &text, mode);
        for flag in FLAGS {
            assert_eq!(read_credentials(flag, &path), want, "{flag} {name}");
        }
    }
}

/// Asserts that a file that starts with `{` and is not the JSON of exactly `access_key_id` and
/// `secret_access_key`, both non-empty strings, is refused with the flag, the path and the form
/// that is wanted: text that stops short, a missing field, a field misspelled, a field besides
/// the two, a value of another type, an empty value, a field twice, text after the object. No
/// message carries the secret, nor a value of the wrong type, which a parser would quote.
#[test]
fn json_that_is_not_the_key_pair_is_refused_without_the_secret() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-keys-json");
    let field = secret_field();
    let names = [
        "unclosed",
        "no_id",
        "no_secret",
        "typo",
        "extra",
        "number",
        "null",
        "empty_id",
        "empty_secret",
        "twice",
        "trailing",
        "brace",
    ];
    let texts = [
        format!("{{\"access_key_id\": \"AKID\", {field}"),
        format!("{{{field}}}"),
        String::from("{\"access_key_id\": \"AKID\"}"),
        format!("{{\"access_key_id\": \"AKID\", \"secret_acess_key\": \"{SECRET}\"}}"),
        format!("{{\"access_key_id\": \"AKID\", {field}, \"note\": \"x\"}}"),
        String::from("{\"access_key_id\": \"AKID\", \"secret_access_key\": 987654321}"),
        String::from("{\"access_key_id\": null, \"secret_access_key\": \"x\"}"),
        format!("{{\"access_key_id\": \"\", {field}}}"),
        String::from("{\"access_key_id\": \"AKID\", \"secret_access_key\": \"\"}"),
        format!("{{\"access_key_id\": \"A\", \"access_key_id\": \"B\", {field}}}"),
        format!("{} {SECRET}", json()),
        String::from("{"),
    ];
    assert_eq!(names.len(), texts.len());
    for (name, text) in names.into_iter().zip(texts) {
        let path = file(&dir, name, &text, 0o600);
        let why = read_credentials("--read-only-key-file", &path).unwrap_err();
        assert!(why.starts_with("--read-only-key-file "), "{name}: {why}");
        assert!(why.contains(name), "{name}: {why}");
        assert!(why.contains("access_key_id"), "{name}: {why}");
        assert!(why.contains("secret_access_key"), "{name}: {why}");
        assert!(!why.contains(SECRET), "{name}: {why}");
        assert!(!why.contains("987654321"), "{name}: {why}");
    }
}

/// Asserts that a credentials file that others, or its group, can read or run is refused in
/// both forms, with the flag and the fact, and without the secret, whatever the bits are; and
/// that a file with no bit for the group and others is read.
#[test]
fn a_credentials_file_that_others_can_read_is_refused_in_both_forms() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-keys-modes");
    let lines = format!("AKID\n{SECRET}\n");
    for (name, text) in [("lines", lines), ("json", json())] {
        for mode in [0o644, 0o640, 0o604, 0o660, 0o666, 0o610, 0o601, 0o750] {
            let path = file(&dir, &format!("{name}-{mode:o}"), &text, mode);
            let why = read_credentials("--access-key-file", &path).unwrap_err();
            let at = format!("{name} {mode:o}: {why}");
            assert!(why.starts_with("--access-key-file "), "{at}");
            assert!(why.contains("others can read it"), "{at}");
            assert!(!why.contains(SECRET), "{at}");
        }
        for mode in [0o600, 0o400, 0o700] {
            let path = file(&dir, &format!("ok-{name}-{mode:o}"), &text, mode);
            let read = read_credentials("--access-key-file", &path);
            assert!(read.is_ok(), "{name} {mode:o}");
        }
    }
}

/// Asserts that a two-line file that lacks a line or has an empty one is refused with the form
/// that is wanted, and that a file that is missing, a directory and a file that is not text are
/// refused with the flag and the path, whichever flag it is.
#[test]
fn a_file_that_is_no_key_is_refused_with_the_flag_and_the_path() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-keys-bad");
    for (name, text) in [("one", "AKID\n"), ("no_id", "\nsecret\n"), ("empty", "")] {
        let path = file(&dir, name, text, 0o600);
        let why = read_credentials("--cold-credentials-file", &path).unwrap_err();
        assert!(why.starts_with("--cold-credentials-file "), "{why}");
        assert!(why.contains(name), "{why}");
        assert!(why.contains("on the second"), "{why}");
    }
    let binary = dir.path().join("binary");
    std::fs::write(&binary, [0xff_u8, 0xfe, 0x00]).unwrap();
    std::fs::set_permissions(&binary, Permissions::from_mode(0o600)).unwrap();
    let missing = dir.path().join("missing");
    let folder = dir.path().join("folder");
    std::fs::create_dir(&folder).unwrap();
    std::fs::set_permissions(&folder, Permissions::from_mode(0o700)).unwrap();
    for (name, path) in [("binary", binary), ("missing", missing), ("folder", folder)] {
        let why = read_credentials("--hot-credentials-file", &path).unwrap_err();
        assert!(why.starts_with("--hot-credentials-file "), "{why}");
        assert!(why.contains(name), "{why}");
    }
}

/// Asserts that the read-only keys are read from their files in the order given, in either
/// form, that no file is no key, and that a file that is bad is refused under the name of the
/// flag of the read-only keys, with the path.
#[test]
fn the_read_only_keys_are_read_in_order_and_a_bad_file_is_refused() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-keys-list");
    let first = file(&dir, "first", "ONE\none-secret\n", 0o600);
    let second = file(&dir, "second", &json(), 0o400);
    let keys = read_only_keys(&[first.clone(), second.clone()]).unwrap();
    let want = [
        AccessKey::new("ONE", "one-secret"),
        AccessKey::new("AKID", SECRET),
    ];
    assert_eq!(keys, want);
    assert_eq!(read_only_keys(&[]), Ok(Vec::new()));
    let bad = file(&dir, "bad", "{", 0o600);
    let why = read_only_keys(&[first, bad, second]).unwrap_err();
    assert!(why.starts_with("--read-only-key-file "), "{why}");
    assert!(why.contains("bad"), "{why}");
}

/// Asserts that two keys with one id are refused, whether the second is a read-only key with
/// the id of the full key or of another read-only key, with a message that names the id and the
/// flags and not the secrets, and that keys with different ids, and none, pass.
#[test]
fn two_keys_with_one_id_are_refused() {
    let one = AccessKey::new("ONE", "one-secret");
    let two = AccessKey::new("TWO", "two-secret");
    assert_eq!(ensure_distinct("FULL", &[]), Ok(()));
    let distinct = [one.clone(), two.clone()];
    assert_eq!(ensure_distinct("FULL", &distinct), Ok(()));
    let as_full = AccessKey::new("FULL", "other-secret");
    let cases = [
        ("FULL", vec![as_full]),
        ("FULL", vec![one.clone(), two.clone(), one.clone()]),
        ("ONE", vec![two, one]),
    ];
    for (full, read_only) in cases {
        let why = ensure_distinct(full, &read_only).unwrap_err();
        assert!(why.contains("used twice"), "{why}");
        assert!(why.contains("--access-key-file"), "{why}");
        assert!(why.contains("--read-only-key-file"), "{why}");
        assert!(!why.contains("secret"), "{why}");
    }
}

/// Asserts that the text of a key, as a debug print shows it, has its id and not its secret.
#[test]
fn the_debug_text_of_a_key_hides_the_secret() {
    let shown = format!("{:?}", AccessKey::new("AKID", SECRET));
    assert!(shown.contains("AKID"), "{shown}");
    assert!(!shown.contains(SECRET), "{shown}");
}

/// Asserts that a key is found by its id with its secret and its role: the full key, each
/// read-only key, nothing for an id no key has, and the full key for an id that a read-only key
/// repeats, so that a configuration that was not checked cannot take the full key's id for a
/// reader's.
#[test]
fn a_key_is_found_by_its_id_with_its_secret_and_role() {
    let clock = Arc::new(TestClock::new());
    let mut config = ServerConfig::new("b", "FULL", "full-secret", clock);
    config.read_only_keys = vec![
        AccessKey::new("ONE", "one-secret"),
        AccessKey::new("TWO", "two-secret"),
        AccessKey::new("FULL", "reader-secret"),
    ];
    assert_eq!(config.key("FULL"), Some(("full-secret", Role::Full)));
    assert_eq!(config.key("ONE"), Some(("one-secret", Role::ReadOnly)));
    assert_eq!(config.key("TWO"), Some(("two-secret", Role::ReadOnly)));
    assert_eq!(config.key("NONE"), None);
    assert_eq!(config.key(""), None);
}

/// Asserts that the full key may do a call of any method, and that a read-only key may do the
/// `GET` and the `HEAD` and is refused every other method with the refusal of a read-only key,
/// which is 403 `AccessDenied`.
#[test]
fn a_read_only_key_may_only_get_and_head() {
    let reads = [Method::GET, Method::HEAD];
    let others = [
        Method::PUT,
        Method::POST,
        Method::DELETE,
        Method::PATCH,
        Method::OPTIONS,
    ];
    for method in reads.iter().chain(&others) {
        assert_eq!(Role::Full.permit(method), Ok(()), "{method}");
    }
    for method in &reads {
        assert_eq!(Role::ReadOnly.permit(method), Ok(()), "{method}");
    }
    for method in &others {
        let refused = Role::ReadOnly.permit(method);
        assert_eq!(refused, Err(READ_ONLY), "{method}");
    }
    assert_eq!(READ_ONLY.code, "AccessDenied");
}

/// Asserts which roles may list the open multipart uploads, under any prefix: the full and the
/// read-only role may, and a presigning key may not.
#[test]
fn only_a_presigning_key_is_refused_the_listing_of_open_uploads() {
    for prefix in ["", "jobs/", "_journal/"] {
        assert_eq!(Role::Full.permit_list_uploads(prefix), Ok(()), "{prefix}");
        assert_eq!(
            Role::ReadOnly.permit_list_uploads(prefix),
            Ok(()),
            "{prefix}"
        );
        let refused = Role::PresignPut.permit_list_uploads(prefix);
        assert_eq!(refused, Err(PUT_ONLY), "{prefix}");
    }
}

/// Asserts that a GC key may list the open uploads under its own prefix, at the prefix and
/// deeper, and only there: no prefix, a prefix outside, a prefix that covers its own, and a
/// reserved one are out of scope.
#[test]
fn a_gc_key_may_list_the_open_uploads_under_its_prefix_only() {
    let role = Role::Gc("cas/b/".to_owned());
    for inside in ["cas/b/", "cas/b/jobs/", "cas/b/x"] {
        assert_eq!(role.permit_list_uploads(inside), Ok(()), "{inside}");
    }
    for outside in ["", "cas/", "cas/c/", "cas/b", "_journal/", "_tiering/x"] {
        let refused = role.permit_list_uploads(outside);
        assert_eq!(refused, Err(OUT_OF_SCOPE), "{outside}");
    }
    let reserved = Role::Gc("_journal/".to_owned());
    assert_eq!(
        reserved.permit_list_uploads("_journal/x"),
        Err(OUT_OF_SCOPE)
    );
}
