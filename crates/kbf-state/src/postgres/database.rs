//! A database of its own for one test, with the schema `kbf-installer setup` leaves.
//!
//! Test support (feature `test-database`). kbf-testkit makes the database on the server
//! `KBF_TEST_POSTGRES_URL` names and drops it with the guard; this module applies the schema,
//! which kbf-testkit, with no kbf dependency, cannot.

pub use kbf_testkit::database::TestDatabase;

use super::PostgresConfig;
use super::schema::{self, Migration};

/// A new database on the test server with the state schema applied at its newest version, in
/// the default schema (`kbf_state`) that a flag-opened store lives in. Its
/// [`url`](TestDatabase::url) opens a store, and can be handed to a process under test.
///
/// # Panics
/// When the test server is not named (see [`TestDatabase::empty`]), or refuses the database or
/// the schema.
#[must_use]
pub fn test_database() -> TestDatabase {
    test_database_with(Vec::new())
}

/// [`test_database`] with the migrations of the crates that extend the schema after this
/// crate's own ([`schema::extend`]).
///
/// # Panics
/// As [`test_database`], and when `more` does not continue the numbering.
#[must_use]
pub fn test_database_with(more: Vec<Migration>) -> TestDatabase {
    let set = schema::extend(schema::embedded(), more).expect("the migrations are numbered");
    let database = TestDatabase::empty();
    let config = PostgresConfig::parse(&database.url()).expect("the test database URL parses");
    // The installer creates the schema in a deployment; a test plays the installer.
    schema::apply(&config, &set, schema::latest_of(&set))
        .expect("the test database takes the schema");
    database
}
