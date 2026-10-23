//! The rules that need no database: what is refused and what fails, and the exit status of
//! each, the target's emptiness and clock limits, and the comparison of a read-back.

mod support;

use std::io;

use kbf_state_copy::Error;
use kbf_state_copy::digest::{Entry, summarize};
use kbf_state_copy::target::{self, MAX_CLOCK_SKEW_US, Target};
use support::UNREACHABLE;

fn entry(key: &str, value: &[u8]) -> Entry {
    Entry {
        key: key.to_owned(),
        value: value.to_vec(),
        version: 1,
        expires_us: None,
    }
}

/// The error of a `Result` whose Ok type has no `Debug`.
fn error_of<T>(result: Result<T, Error>) -> Error {
    match result {
        Ok(_) => panic!("an error was expected"),
        Err(error) => error,
    }
}

/// Asserts that a refusal ends a run with status 2 and a failure with status 1, and that
/// each says which it is.
#[test]
fn a_refusal_is_status_2_and_a_failure_is_status_1() {
    let refused = Error::Refused("the lock is held".to_owned());
    let failed = Error::Failed("the call broke".to_owned());
    assert_eq!(refused.code(), 2);
    assert_eq!(failed.code(), 1);
    assert_eq!(refused.to_string(), "refused: the lock is held");
    assert_eq!(failed.to_string(), "failed: the call broke");
}

/// Asserts that an I/O error, a SQLite error and a URL the store does not take are each a
/// failure, and that each says where it came from.
#[test]
fn errors_from_the_libraries_are_failures() {
    let closed: Error = io::Error::other("the pipe is closed").into();
    assert_eq!(closed.to_string(), "failed: the pipe is closed");
    let sqlite: Error = rusqlite::Error::InvalidQuery.into();
    assert_eq!(sqlite.code(), 1);
    assert!(
        sqlite.to_string().starts_with("failed: the state file: "),
        "{sqlite}"
    );
    for (url, said) in [
        ("", "needs a URL"),
        ("mysql://h/db", "must start with postgres://"),
        ("postgres:///db", "names no host"),
        ("postgres://h/db?sslmode=require", "TLS"),
    ] {
        let error = error_of(Target::new(url));
        assert_eq!(error.code(), 1, "{url}");
        assert!(error.to_string().contains(said), "{url}: {error}");
    }
}

/// Asserts that a store that holds anything is refused, and one that holds nothing is not.
#[test]
fn a_target_that_holds_anything_is_refused() {
    assert!(target::require_empty(0).is_ok());
    for held in [1, 2, 1_000] {
        let error = target::require_empty(held).expect_err("a store with data is refused");
        assert_eq!(error.code(), 2);
        assert!(error.to_string().contains("already holds data"), "{error}");
    }
}

/// Asserts that clocks up to 2 seconds apart, either way round, are accepted, and that
/// clocks further apart are refused with the gap in whole seconds.
#[test]
fn clocks_more_than_two_seconds_apart_are_refused() {
    let now = 1_700_000_000_000_000_i64;
    let limit = i64::try_from(MAX_CLOCK_SKEW_US).expect("a small limit");
    assert_eq!(limit, 2_000_000);
    assert!(target::require_clocks(now, now).is_ok());
    assert!(target::require_clocks(now + limit, now).is_ok());
    assert!(target::require_clocks(now, now + limit).is_ok());
    let ahead = target::require_clocks(now + limit + 1, now).expect_err("ahead");
    assert_eq!(ahead.code(), 2);
    let said = ahead.to_string();
    assert!(said.contains("differ by 2 s; the limit is 2 s"), "{said}");
    let behind = target::require_clocks(now, now + 7_500_000).expect_err("behind");
    assert!(behind.to_string().contains("differ by 7 s"), "{behind}");
}

/// Asserts that a read-back that matches is accepted, and one that does not is a failure that
/// names the family and says what the mismatch means at that point.
#[test]
fn a_read_back_that_differs_is_a_failure_naming_the_family() {
    let state = summarize(&[entry("op/1", b"a")], &[]);
    let same = summarize(&[entry("op/1", b"a")], &[]);
    let other = summarize(&[entry("op/1", b"b")], &[]);
    assert!(target::verify(&state, &same, target::IN_TRANSACTION).is_ok());
    let error = target::verify(&state, &other, target::IN_TRANSACTION).expect_err("differs");
    assert_eq!(error.code(), 1);
    let text = error.to_string();
    assert!(text.contains("state file before the commit"), "{text}");
    assert!(text.contains("key family op/"), "{text}");
    assert!(text.contains("rolled back"), "{text}");
    let error = target::verify(&state, &other, target::AFTER_COMMIT).expect_err("differs");
    assert!(error.to_string().contains("after the commit; the target holds a copy"));
}

/// Asserts that a server that cannot be reached is a failure from every step that connects,
/// and that no error text carries the password of the URL.
#[test]
fn a_server_that_cannot_be_reached_is_a_failure_that_never_prints_the_password() {
    let url = "postgres://nobody:s3cret-pw@127.0.0.1:1/none";
    let target = Target::new(url).unwrap_or_else(|error| panic!("the URL is fine: {error}"));
    let connect = error_of(target.connect());
    let said = connect.to_string();
    assert!(said.starts_with("failed: the target: "), "{said}");
    let schema = target.check_schema().expect_err("no server");
    let probe = error_of(target.probe());
    for error in [connect, schema, probe] {
        assert_eq!(error.code(), 1);
        assert!(!error.to_string().contains("s3cret-pw"), "{error}");
    }
    let plain = Target::new(UNREACHABLE).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(plain.check_schema().expect_err("no server").code(), 1);
}
