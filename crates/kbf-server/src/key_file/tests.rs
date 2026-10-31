//! The key file: what it must be, what is read from it, and what is never said about it.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use clap::Parser;
use tracing::Level;

use super::*;
use crate::test_dir::scratch_dir;
use crate::test_log::capture_at;

const SECRET: &str = "file-secret-7c41";
const ENV_SECRET: &str = "env-secret-93ab";

fn pair(id: &str, secret: &str) -> String {
    format!(r#"{{"access_key_id": "{id}", "secret_access_key": "{secret}"}}"#)
}

/// A file `name` in `dir` holding `text` at `mode`.
fn file(dir: &Path, name: &str, text: &str, mode: u32) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).expect("written");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("mode set");
    path
}

fn env(name: &str) -> Option<String> {
    match name {
        "AWS_ACCESS_KEY_ID" => Some("ENVKEYID".to_owned()),
        "AWS_SECRET_ACCESS_KEY" => Some(ENV_SECRET.to_owned()),
        _ => None,
    }
}

fn no_env(_: &str) -> Option<String> {
    None
}

/// The refusal of `path` as the cold store's key file, as text; also checks that its
/// `Debug` says no more than its text does about the secret.
fn refused(path: &Path) -> String {
    let error = cold_keys(Some(path), no_env).expect_err("refused");
    let said = error.to_string();
    let debug = format!("{error:?}");
    for text in [&said, &debug] {
        assert!(!text.contains(SECRET), "{text}");
        assert!(!text.contains("hunter2"), "{text}");
    }
    assert!(said.starts_with("--cold-store-credentials-file "), "{said}");
    assert!(said.contains(&path.display().to_string()), "{said}");
    said
}

/// Catches: a key file others can read, accepted (0644, 0640, 0660 and 0700 are refused, the mode named and the path, never the secret); 0600 and 0400 accepted.
#[test]
fn cold_store_key_file_must_be_owner_only() {
    let dir = scratch_dir("cold-key-mode");
    for private in [0o600, 0o400] {
        let path = file(
            dir.path(),
            "private.json",
            &pair("FILEKEYID", SECRET),
            private,
        );
        let keys = cold_keys(Some(&path), no_env).expect("read");
        let printed = format!("{keys:?}");
        assert!(printed.contains("FILEKEYID"), "{printed}");
        assert!(!printed.contains(SECRET), "{printed}");
    }
    for mode in [0o644, 0o640, 0o660, 0o700] {
        let path = file(dir.path(), "wide.json", &pair("FILEKEYID", SECRET), mode);
        let said = refused(&path);
        assert!(said.contains(&format!("mode {mode:04o}")), "{said}");
    }
}

/// Catches: a symbolic link followed to a key file (a link planted where the operator
/// expected a file would let someone else choose which secret is read).
#[test]
fn cold_store_key_file_that_is_a_symlink_is_refused() {
    let dir = scratch_dir("cold-key-link");
    let target = file(dir.path(), "real.json", &pair("FILEKEYID", SECRET), 0o600);
    let link = dir.path().join("link.json");
    std::os::unix::fs::symlink(&target, &link).expect("link");
    refused(&link);
    assert!(cold_keys(Some(&target), no_env).is_ok());
}

/// Catches: a directory read as if it were a key file, and a missing file that says nothing
/// of its path.
#[test]
fn cold_store_key_file_must_be_a_regular_file_that_exists() {
    let dir = scratch_dir("cold-key-kind");
    let said = refused(dir.path());
    assert!(said.contains("not a regular file"), "{said}");
    refused(&dir.path().join("absent.json"));
}

/// Catches: file content echoed in an error (the serde message of a wrong type repeats the
/// value), and content that is not the pair, holds an empty key or is far too long, accepted.
#[test]
fn cold_store_key_file_content_is_never_echoed() {
    let dir = scratch_dir("cold-key-content");
    let cases = [
        "hunter2 is not JSON".to_owned(),
        r#"{"access_key_id": "hunter2", "secret_access_key": 7}"#.to_owned(),
        format!(r#"{{"access_key_id": "hunter2", "secret": "{SECRET}"}}"#),
        format!(r#"{{"access_key_id": {{"hunter2": "{SECRET}"}}, "secret_access_key": "x"}}"#),
        pair("", SECRET),
        pair("hunter2", ""),
        String::new(),
        // Longer than a key file may be, whatever it holds.
        format!("{}{SECRET}", " ".repeat(8192)),
    ];
    for text in cases {
        let path = file(dir.path(), "bad.json", &text, 0o600);
        let said = refused(&path);
        assert!(said.contains("not JSON"), "{said}");
    }
}

/// Catches: the environment consulted, or warned about, when the file is given.
#[test]
fn the_key_file_wins_over_the_environment_and_logs_no_warning() {
    let dir = scratch_dir("cold-key-wins");
    let path = file(dir.path(), "keys.json", &pair("FILEKEYID", SECRET), 0o600);
    let log = capture_at(Level::WARN);
    let keys = cold_keys(Some(&path), env).expect("read");
    let printed = format!("{keys:?}");
    assert!(printed.contains("FILEKEYID"), "{printed}");
    assert!(!printed.contains("ENVKEYID"), "{printed}");
    assert_eq!(log.text(), "");
}

/// Catches: the environment fallback removed too early (pilot units rely on it for one
/// release), no warning, a warning on every call, and a warning that names a value.
#[test]
fn environment_keys_still_work_and_log_one_deprecation_warning_without_a_value() {
    let log = capture_at(Level::WARN);
    let keys = cold_keys(None, env).expect("the environment's pair");
    let printed = format!("{keys:?}");
    assert!(printed.contains("ENVKEYID"), "{printed}");
    let text = log.text();
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.contains("deprecated"), "{text}");
    assert!(text.contains("--cold-store-credentials-file"), "{text}");
    assert!(!text.contains(ENV_SECRET), "{text}");
    assert!(!text.contains("ENVKEYID"), "{text}");
}

/// Catches: a start with no keys at all that does not say which variable is missing (the
/// frontend's registration may still supply them, and its start reads the name), and a
/// deprecation warning for a pair that was never used.
#[test]
fn no_file_and_no_environment_names_the_missing_variable_and_warns_of_nothing() {
    let log = capture_at(Level::WARN);
    let Err(ConfigError::MissingEnv(name)) = cold_keys(None, no_env) else {
        panic!("a missing pair is not a pair");
    };
    assert_eq!(name, "AWS_ACCESS_KEY_ID");
    let half = |name: &str| (name == "AWS_ACCESS_KEY_ID").then(|| "ENVKEYID".to_owned());
    let Err(ConfigError::MissingEnv(name)) = cold_keys(None, half) else {
        panic!("half a pair is not a pair");
    };
    assert_eq!(name, "AWS_SECRET_ACCESS_KEY");
    assert_eq!(log.text(), "");
}

/// Catches: a key file with a `file://` cold store accepted (it would do nothing), the
/// flag taken without a cold store, and the check refusing an `s3://` store or no file.
#[test]
fn file_cold_store_refuses_a_credentials_file() {
    let parse = |args: &[&str]| {
        Args::try_parse_from(std::iter::once("kbf-server").chain(args.iter().copied()))
    };
    let s3 = "--cold-store=s3://b?endpoint=http://127.0.0.1:9";
    let key = "--cold-store-credentials-file=/etc/kbf/cold.json";
    assert!(
        parse(&["--cold-store=file:///srv/cold"])
            .expect("flags")
            .check_cold_key_file()
            .is_ok()
    );
    assert!(parse(&[s3]).expect("flags").check_cold_key_file().is_ok());
    assert!(
        parse(&[s3, key])
            .expect("flags")
            .check_cold_key_file()
            .is_ok()
    );
    let on_file = parse(&["--cold-store=file:///srv/cold", key]).expect("flags");
    let error = on_file.check_cold_key_file().expect_err("refused");
    assert!(
        matches!(error, ConfigError::ColdKeyFileOnFileStore),
        "{error}"
    );
    assert!(error.to_string().contains("--cold-store-credentials-file"));
    // clap: the flag needs `--cold-store`.
    assert!(parse(&[key]).is_err());
}

/// Catches: the key file's path lost on its way from the flag to the store's keys.
#[test]
fn the_flag_names_the_file() {
    let args = Args::try_parse_from([
        "kbf-server",
        "--cold-store=s3://b?endpoint=http://127.0.0.1:9",
        "--cold-store-credentials-file=/etc/kbf/cold.json",
    ])
    .expect("flags");
    assert_eq!(
        args.cold_store_credentials_file,
        Some(PathBuf::from("/etc/kbf/cold.json"))
    );
    let args = Args::try_parse_from(["kbf-server", "--cold-store=file:///c"]).expect("flags");
    assert_eq!(args.cold_store_credentials_file, None);
}

/// Catches: a reason that repeats the path (the message would name it twice) or says nothing
/// of what is wrong, for each way a secret file is refused.
#[test]
fn every_secret_file_refusal_has_a_reason_without_the_path() {
    use crate::token::TokenFileError;
    let path = PathBuf::from("/p/secret-name");
    let cases = [
        (
            TokenFileError::NotAFile { path: path.clone() },
            "not a regular file",
        ),
        (
            TokenFileError::Owner {
                path: path.clone(),
                owner: 1,
                server: 2,
            },
            "owned by uid 1, not by this server's uid 2",
        ),
        (
            TokenFileError::Mode {
                path: path.clone(),
                mode: 0o644,
            },
            "mode 0644",
        ),
        (
            TokenFileError::Content { path: path.clone() },
            "content is not usable",
        ),
        (
            TokenFileError::Read {
                path: path.clone(),
                source: std::io::ErrorKind::NotFound.into(),
            },
            "entity not found",
        ),
    ];
    for (error, says) in cases {
        let reason = error.reason();
        assert!(reason.contains(says), "{reason}");
        assert!(!reason.contains("secret-name"), "{reason}");
    }
}
