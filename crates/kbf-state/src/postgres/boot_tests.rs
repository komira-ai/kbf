//! The schema step of `kbf boot-check` ([`super`]) without a server: what a step of this build
//! holds, and the refusals that come before or without a connection. The step against a real
//! server is `tests/postgres_boot.rs`.

use super::{BootStep, LOCK_WAIT, said};
use crate::postgres::PostgresConfig;
use crate::postgres::schema::{self, Outcome, SchemaError};

/// The step of this build over a socket directory that does not exist.
fn step_without_a_server() -> BootStep {
    let config = PostgresConfig::parse("postgres:///kbf?host=/nonexistent-kbf-test-socket")
        .expect("a usable URL");
    BootStep::new(config, schema::embedded(), LOCK_WAIT)
}

/// Asserts that the step of this build waits for a lock at most `LOCK_WAIT` (30 s) and holds
/// the migrations this build embeds.
#[test]
fn the_step_of_this_build_holds_its_migrations_and_the_lock_wait() {
    let shown = format!("{:?}", step_without_a_server());
    assert!(shown.contains("lock_wait: 30s"), "{shown}");
    assert_eq!(LOCK_WAIT.as_secs(), 30);
    let embedded = schema::embedded();
    assert!(!embedded.is_empty());
    assert!(shown.contains(&format!("{embedded:?}")), "{shown}");
}

/// Asserts that a step asked for a schema its migrations do not embed is refused with the
/// line that says so, and that the refusal is one line and comes without reaching for the
/// server (the socket directory is missing, and the answer is not about it).
#[test]
fn a_schema_the_build_does_not_embed_is_refused_without_a_connection() {
    let wanted = schema::latest() + 1;
    let why = step_without_a_server()
        .migrate_to(wanted)
        .expect_err("not embedded");
    let says = format!("needs state schema {wanted}");
    assert!(why.contains(&says), "{why}");
    assert!(!why.contains('\n'), "{why:?}");
    assert!(!why.contains("installer"), "{why}");
}

/// Asserts that a database ahead of the build is not a refusal of the step: the line says
/// nothing was applied, and it names no installer package, so that a kbf package rolled back
/// to an earlier release starts on the schema its successor left.
#[test]
fn a_database_ahead_of_the_build_is_nothing_to_apply() {
    let ahead = SchemaError::DatabaseAhead {
        found: 2,
        newest: 1,
    };
    let line = said(Err(ahead)).expect("not a refusal");
    assert!(line.contains("nothing to apply"), "{line}");
    assert!(line.contains("version 2"), "{line}");
    assert!(!line.contains("installer"), "{line}");
}

/// Asserts that a schema the migrations do not embed is a refusal in the words of a kbf
/// package, which name the schema asked for and not the installer package.
#[test]
fn a_schema_not_shipped_is_refused_without_naming_the_installer() {
    let not_shipped = SchemaError::NotShipped {
        wanted: 3,
        newest: 1,
    };
    let why = said(Err(not_shipped)).expect_err("a refusal");
    assert!(why.contains("needs state schema 3"), "{why}");
    assert!(!why.contains("installer"), "{why}");
}

/// Asserts that every other outcome of the schema code is passed on as it says itself: a
/// success as its own line, and a busy schema as the refusal the schema code words.
#[test]
fn the_other_outcomes_are_passed_on_in_their_own_words() {
    let done = Outcome {
        from: Some(1),
        to: 1,
        ran: Vec::new(),
    };
    let line = said(Ok(done)).expect("a success");
    assert_eq!(line, "the schema is at version 1, nothing to apply");
    let why = said(Err(SchemaError::Busy)).expect_err("busy");
    assert_eq!(why, SchemaError::Busy.to_string());
}

/// Asserts that a step whose server cannot be reached refuses with the database's own
/// one line, so that the unit does not start on a database nobody could check.
#[test]
fn a_server_that_cannot_be_reached_is_a_refusal_of_one_line() {
    let why = step_without_a_server()
        .migrate_to(schema::latest())
        .expect_err("no server");
    assert!(why.starts_with("state database: "), "{why}");
    assert!(!why.contains('\n'), "{why:?}");
}
