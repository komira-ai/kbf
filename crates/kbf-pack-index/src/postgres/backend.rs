//! The conformance suite's view of a Postgres database for the pack index: every index it
//! opens gets a schema of its own in the one database (the migrations a kbf release embeds,
//! [`schema::embedded`], applied), dropped when the backend is.
//!
//! Test support. The URL names a server the tests may create and drop schemas on.
//!
//! The state store's own test backend (`kbf_state::postgres::PostgresBackend`) is not used:
//! it applies the state store's whole set and puts an extension's migrations after it, and
//! the state store's migration 2 is not the one a kbf database recorded as 2 (the module
//! docs of [`schema`]).

use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use kbf_state::ManualClock;
use kbf_state::postgres::PostgresConfig;
use kbf_state::postgres::schema as state_schema;

use super::PostgresPackIndex;
use crate::conformance::PackBackend;
use crate::schema;

/// Indexes opened by any backend in this process get distinct schemas.
static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

/// Opens pack indexes on one Postgres database, one schema each.
pub struct PostgresPackBackend {
    url: String,
    base: PostgresConfig,
    schemas: Mutex<Vec<String>>,
}

impl PostgresPackBackend {
    /// A backend on the database at `url`.
    ///
    /// # Panics
    /// When the URL does not parse.
    #[must_use]
    pub fn new(url: &str) -> PostgresPackBackend {
        PostgresPackBackend {
            url: url.to_owned(),
            base: PostgresConfig::parse(url)
                .expect("the test database URL is a usable postgres:// URL"),
            schemas: Mutex::new(Vec::new()),
        }
    }

    /// A configuration for a new schema with the pack tables, recorded for dropping.
    ///
    /// # Panics
    /// When the server refuses the schema or a migration.
    #[must_use]
    pub fn fresh_config(&self) -> PostgresConfig {
        let n = NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed);
        let name = format!("kbf_p{}_{n}", process::id());
        let config = self
            .base
            .clone()
            .with_schema(&name)
            .expect("the generated schema name is plain")
            .with_application_name(&name);
        // The installer creates the schema in a deployment; a test plays the installer.
        state_schema::apply(&config, &schema::embedded(), schema::latest())
            .expect("the test database takes the schema");
        self.schemas
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(name);
        config
    }
}

impl Drop for PostgresPackBackend {
    fn drop(&mut self) {
        let schemas = self
            .schemas
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        // Best effort, over a plain connection as the state store's backend makes: a schema
        // left behind is only clutter in a throwaway database.
        let _ = self
            .url
            .parse::<postgres::Config>()
            .and_then(|config| config.connect(postgres::NoTls))
            .map(|mut client| {
                for schema in schemas.iter() {
                    let _ =
                        client.batch_execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"));
                }
            });
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
