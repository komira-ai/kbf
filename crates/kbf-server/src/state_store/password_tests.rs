//! The password of a `postgres://` `--state-store` (`Args::with_state_store_password`).

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use clap::Parser;
use kbf_pg_auth::{AuthError, Context, FileFlaw};
use kbf_testkit::dir::TestDir;

use super::*;
use crate::Args;
use crate::test_log::capture_at;

const URL: &str = "--state-store=postgres://kbf@db.example/kbf?sslmode=require";

/// A context with this process's uid and nothing in the environment but libpq's file.
fn context(passfile: Option<PathBuf>) -> Context {
    Context {
        pgpassword: None,
        passfile,
        euid: rustix::process::geteuid().as_raw(),
        user: Some("kbf".to_owned()),
    }
}

fn args(flags: &[&str]) -> Args {
    Args::try_parse_from(std::iter::once("kbf-server").chain(flags.iter().copied()))
        .expect("the flags parse")
}

fn secret(dir: &Path, name: &str, text: &str, mode: u32) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    path
}

/// Whether the Postgres store of `args` connects with a password.
fn has_password(args: &Args) -> bool {
    match &args.state_store {
        Some(StateStoreUri::Postgres(url)) => url.config.has_password(),
        other => panic!("not a Postgres store: {other:?}"),
    }
}

/// Catches the password file's password not reaching the store's connections, the URL's text
/// changed by it (two frontends compare the text), the start not saying where the password came
/// from, and the password itself in the log.
#[test]
fn the_password_file_gives_the_postgres_store_its_password_and_the_log_says_so() {
    let dir = TestDir::under(std::env::temp_dir(), "state-store-password-file");
    let file = secret(&dir, "pw", "hunter2\n", 0o600);
    let flags = args(&[URL, &format!("--state-store-password-file={}", file.display())]);
    assert!(!has_password(&flags));
    let log = capture_at(tracing::Level::INFO);
    let authed = flags
        .with_state_store_password(&context(None))
        .expect("authenticated");
    assert!(has_password(&authed));
    assert_eq!(authed.state_store, flags.state_store);
    let text = log.text();
    let want = format!("state store password: the password file {}", file.display());
    assert!(text.contains(&want), "{text}");
    assert!(!text.contains("hunter2"), "{text}");
}

/// Catches libpq's file skipped (group-readable) without a warning, a password taken from it
/// anyway, and, with no source at all, a password invented or the start refused.
#[test]
fn a_libpq_file_libpq_would_skip_is_a_warning_and_no_password() {
    let dir = TestDir::under(std::env::temp_dir(), "state-store-pgpass-skip");
    let pgpass = secret(&dir, "pgpass", "*:*:*:*:hunter2\n", 0o644);
    let log = capture_at(tracing::Level::INFO);
    let authed = args(&[URL])
        .with_state_store_password(&context(Some(pgpass.clone())))
        .expect("no password, and no refusal");
    assert!(!has_password(&authed));
    let text = log.text();
    assert!(text.contains("state store password: no password"), "{text}");
    let warning = format!("state store password: {} is skipped, as libpq skips it", pgpass.display());
    assert!(text.contains(&warning), "{text}");
    let authed = args(&[URL])
        .with_state_store_password(&context(None))
        .expect("no password");
    assert!(!has_password(&authed));
}

/// Catches a password file others can read used (or passed over for another source), and a
/// password file beside a SQLite store, which takes none, ignored; each with its full text.
#[test]
fn a_password_file_that_cannot_be_used_or_has_no_postgres_store_is_refused() {
    let dir = TestDir::under(std::env::temp_dir(), "state-store-password-refused");
    let file = secret(&dir, "pw", "hunter2\n", 0o644);
    let flag = format!("--state-store-password-file={}", file.display());
    let refused = args(&[URL, &flag])
        .with_state_store_password(&context(None))
        .expect_err("a file others can read");
    assert!(matches!(
        &refused,
        StateError::Password(AuthError::File { flaw: FileFlaw::Mode(0o644), .. })
    ));
    assert_eq!(
        refused.to_string(),
        format!(
            "the state store's password: the password file {}: it has mode 0644; it must be \
             0600 or 0400",
            file.display()
        )
    );
    let sqlite = args(&["--state-store=sqlite:///var/lib/kbf/state.db", &flag])
        .with_state_store_password(&context(None))
        .expect_err("SQLite takes no password");
    assert!(matches!(sqlite, StateError::PasswordFileForSqlite));
    assert_eq!(
        sqlite.to_string(),
        "--state-store-password-file is for a postgres:// store, and this one is SQLite"
    );
}

/// Catches flags with no Postgres store changed, and a URL built in this process with its
/// password (as the tests' database URL is) looked up again: its password file is not read.
#[test]
fn flags_without_a_postgres_store_or_with_a_password_already_are_left_as_they_are() {
    let none = args(&[]).with_state_store_password(&context(None)).expect("none");
    assert_eq!(none.state_store, None);
    let sqlite = args(&["--state-store=sqlite:///var/lib/kbf/state.db"])
        .with_state_store_password(&context(None))
        .expect("SQLite");
    assert_eq!(
        sqlite.state_store,
        Some(StateStoreUri::Sqlite(PathBuf::from("/var/lib/kbf/state.db")))
    );
    let mut built = args(&["--state-store-password-file=/nonexistent/pw", URL]);
    built.state_store = Some(
        "postgres://kbf:hunter2@db.example/kbf"
            .parse()
            .expect("FromStr takes a password"),
    );
    let kept = built
        .with_state_store_password(&context(None))
        .expect("kept");
    assert!(has_password(&kept));
}

/// Catches a password file given with no store to use it for, which would be read for nothing.
#[test]
fn the_password_file_flag_needs_a_state_store() {
    let refused = Args::try_parse_from(["kbf-server", "--state-store-password-file=/etc/kbf/pw"])
        .expect_err("refused");
    assert_eq!(
        refused.kind(),
        clap::error::ErrorKind::MissingRequiredArgument
    );
}
