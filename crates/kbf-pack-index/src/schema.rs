//! The pack index's migration, and the set of migrations a kbf release runs: the state
//! store's own ([`kbf_state::postgres::schema::embedded`]) followed by this crate's.
//!
//! The migration is number 2, named `pack_index`, an `expand` step; the database records those
//! three and nothing else of it, so they never change. Its file is the one the state store
//! shipped before the pack index moved here, byte for byte, and a database that applied it
//! then is up to date now: the runner finds the row and skips it (`tests/postgres_schema.rs`).
//!
//! A binary that brings a database forward (`kbf-installer`, `kbf boot-check`) or reports
//! the schema it needs takes [`embedded`] and [`latest`] from here, not from `kbf-state`.
//!
//! The state store's set now has a migration 2 of its own (`writer_generation`, the table of
//! its writer-lock fence), whose number is the one every kbf database has recorded for
//! `pack_index`. A kbf release takes the state store's migrations only up to
//! [`STATE_STORE_UPTO`], so a database keeps the numbers and names it recorded. No kbf process
//! takes the state store's writer lock, the one reader of that table, so a kbf database does
//! without it.

use kbf_state::postgres::PostgresConfig;
use kbf_state::postgres::boot::{BootStep, LOCK_WAIT};
use kbf_state::postgres::schema::{self, Migration};

/// The migration's number.
pub const VERSION: u32 = 2;

/// The last of the state store's own migrations a kbf release runs: `state`. Its later ones
/// are numbered from 2, which a kbf database recorded as this crate's.
pub const STATE_STORE_UPTO: u32 = 1;

/// The name recorded in `schema_version`.
pub const NAME: &str = "pack_index";

/// The migration's file.
pub const SQL: &str = include_str!("../migrations/0002_pack_index.sql");

/// The pack index's migration.
///
/// # Panics
/// When the file fails its own lint (a test refuses that, so a release never ships one).
#[must_use]
pub fn migration() -> Migration {
    Migration::new(VERSION, NAME, SQL).expect("the pack index migration lints")
}

/// The migrations a kbf release embeds, in order: the state store's up to
/// [`STATE_STORE_UPTO`], then this crate's.
///
/// # Panics
/// When [`migration`] does not continue the numbering of the state store's (a test refuses
/// that).
#[must_use]
pub fn embedded() -> Vec<Migration> {
    let state = schema::embedded()
        .into_iter()
        .filter(|m| m.version() <= STATE_STORE_UPTO)
        .collect();
    schema::extend(state, [migration()]).expect("the migrations are numbered")
}

/// The newest schema version a kbf release embeds.
#[must_use]
pub fn latest() -> u32 {
    VERSION
}

/// The schema step a kbf release runs before its unit starts (`kbf boot-check`).
#[must_use]
pub fn boot_step(config: PostgresConfig) -> BootStep {
    BootStep::new(config, embedded(), LOCK_WAIT)
}

/// A new database on the test server with the schema a kbf release embeds ([`embedded`])
/// applied, in the default schema a flag-opened store lives in. Test support (feature
/// `test-database`).
///
/// # Panics
/// As [`kbf_state::postgres::test_database`].
#[cfg(feature = "test-database")]
#[must_use]
pub fn test_database() -> kbf_state::postgres::TestDatabase {
    let database = kbf_state::postgres::TestDatabase::empty();
    let config = PostgresConfig::parse(&database.url()).expect("the test database URL parses");
    // The installer creates the schema in a deployment; a test plays the installer.
    schema::apply(&config, &embedded(), latest()).expect("the test database takes the schema");
    database
}
