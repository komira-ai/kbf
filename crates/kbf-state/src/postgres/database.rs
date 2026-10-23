//! A database of its own for one test, with the schema `kbf-installer setup` leaves.
//!
//! Test support (feature `test-database`). kbf-testkit makes the database on the server
//! `KBF_TEST_POSTGRES_URL` names and drops it with the guard; this module applies the schema,
//! which kbf-testkit, with no kbf dependency, cannot.

pub use kbf_testkit::database::TestDatabase;

use super::{PostgresConfig, schema};

/// A new database on the test server with the state schema applied at its newest version, in
/// the default schema (`kbf_state`) that a flag-opened store lives in. Its
/// [`url`](TestDatabase::url) opens a store, and can be handed to a process under test.
///
/// # Panics
/// When the test server is not named (see [`TestDatabase::empty`]), or refuses the database or
/// the schema.
#[must_use]
pub fn test_database() -> TestDatabase {
    let database = TestDatabase::empty();
    let config = PostgresConfig::parse(&database.url()).expect("the test database URL parses");
    // The installer creates the schema in a deployment; a test plays the installer.
    schema::apply(&config, &schema::embedded(), schema::latest())
        .expect("the test database takes the schema");
    database
}
