//! The state schema step of a frontend's start (`state_schema`): what the start does with
//! each answer of the step, over a scripted step.

use std::cell::Cell;

use super::state_schema::{SchemaRefused, migrate_with};
use crate::StateStoreUri;
use crate::test_log::capture;

/// A database behind this build: the step migrates it, the start goes on, and one line says
/// what the step did. The step is asked for the schema this build embeds.
#[test]
fn a_schema_behind_is_migrated_and_the_start_goes_on() {
    let log = capture();
    let asked = Cell::new(None);
    let done = migrate_with(&|wanted| {
        asked.set(Some(wanted));
        Ok("migrated the schema from 3 to 4".to_owned())
    });
    assert!(done.is_ok());
    assert_eq!(asked.get(), Some(kbf_pack_index::schema::latest()));
    let text = log.text();
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(
        text.contains("state schema: migrated the schema from 3 to 4"),
        "{text}"
    );
}

/// A database already at the schema: the step says nothing to apply, the start goes on, and
/// the line says so.
#[test]
fn a_schema_that_is_current_is_a_no_op() {
    let log = capture();
    let done = migrate_with(&|_| Ok("the schema is at version 4, nothing to apply".to_owned()));
    assert!(done.is_ok());
    let text = log.text();
    assert!(
        text.contains("state schema: the schema is at version 4, nothing to apply"),
        "{text}"
    );
}

/// A step that refuses refuses the start, and the message names the cause and the verb that
/// runs the same step.
#[test]
fn a_step_that_refuses_refuses_the_start_naming_the_cause_and_boot_check() {
    let done = migrate_with(&|_| Err("permission denied for schema public".to_owned()));
    let SchemaRefused(why) = done.expect_err("refused");
    assert_eq!(why, "permission denied for schema public");
    assert_eq!(
        SchemaRefused(why).to_string(),
        "the state schema cannot be brought to the one this build needs: permission denied for \
         schema public; `kbf boot-check --mode=frontend` runs the same step and shows it"
    );
}

/// A Postgres store has a step and a SQLite store has none.
#[test]
fn only_a_postgres_store_has_a_schema_step() {
    let postgres: StateStoreUri = "postgres:///kbf?host=/run/postgresql".parse().expect("url");
    let sqlite: StateStoreUri = "sqlite:///var/lib/kbf/state.db".parse().expect("path");
    assert!(postgres.schema_step().is_some());
    assert!(sqlite.schema_step().is_none());
}
