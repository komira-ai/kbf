//! The conformance suite's view of a Postgres database for the pack index: every index it
//! opens gets a schema of its own in the one database (the state schema and the pack
//! migration applied), dropped when the backend is.
//!
//! Test support. The URL names a server the tests may create and drop schemas on.

use std::sync::Arc;

use kbf_state::ManualClock;
use kbf_state::postgres::{Flaw, PostgresBackend, PostgresConfig};

use super::PostgresPackIndex;
use crate::conformance::PackBackend;
use crate::schema;

/// Opens pack indexes on one Postgres database, one schema each.
pub struct PostgresPackBackend {
    schemas: PostgresBackend,
}

impl PostgresPackBackend {
    /// A backend on the database at `url`.
    ///
    /// # Panics
    /// When the URL does not parse.
    #[must_use]
    pub fn new(url: &str) -> PostgresPackBackend {
        PostgresPackBackend {
            schemas: PostgresBackend::new(url, Flaw::None)
                .with_migrations(vec![schema::migration()]),
        }
    }

    /// A configuration for a new schema with the pack tables, recorded for dropping.
    #[must_use]
    pub fn fresh_config(&self) -> PostgresConfig {
        self.schemas.fresh_config()
    }
}

impl PackBackend for PostgresPackBackend {
    type Index = PostgresPackIndex;

    fn open_index(&self, clock: Arc<ManualClock>) -> PostgresPackIndex {
        PostgresPackIndex::open(&self.fresh_config())
            .expect("the test database opens")
            .with_clock(clock)
    }

    fn reconnect(&self, index: &PostgresPackIndex) -> PostgresPackIndex {
        index.reconnect().expect("the test database reopens")
    }
}
