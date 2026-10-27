//! The pack index's migration, and the set of migrations a kbf release runs: the state
//! store's own ([`kbf_state::postgres::schema::embedded`]) followed by this crate's.
//!
//! The migration is number 2, named `pack_index`, an `expand` step; the database records those
//! three and nothing else of it, so they never change. Its file is the one the state store
//! shipped before the pack index moved here, byte for byte, and a database that applied it
//! then is up to date now: the runner finds the row and skips it (`tests/postgres_schema.rs`).
//!
//! A binary that brings a database forward (`kbf-installer`, `kbf boot-check`) or reports
//! the schema it needs takes [`embedded`] and [`latest`] from here, not from `kbf-state`,
//! whose own set stops at migration 1.

use kbf_state::postgres::PostgresConfig;
use kbf_state::postgres::boot::{BootStep, LOCK_WAIT};
use kbf_state::postgres::schema::{self, Migration};

/// The migration's number.
pub const VERSION: u32 = 2;

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

/// The migrations a kbf release embeds, in order.
///
/// # Panics
/// When [`migration`] does not continue the numbering of the state store's (a test refuses
/// that).
#[must_use]
pub fn embedded() -> Vec<Migration> {
    schema::extend(schema::embedded(), [migration()]).expect("the migrations are numbered")
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

/// A new database on the test server with the schema a kbf release embeds applied, in the
/// default schema a flag-opened store lives in: [`kbf_state::postgres::test_database`] with
/// the pack tables. Test support (feature `test-database`).
///
/// # Panics
/// As [`kbf_state::postgres::test_database`].
#[cfg(feature = "test-database")]
#[must_use]
pub fn test_database() -> kbf_state::postgres::TestDatabase {
    kbf_state::postgres::test_database_with(vec![migration()])
}
