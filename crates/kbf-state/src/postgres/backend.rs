//! The conformance suite's view of a Postgres database. Stub: the implementation follows
//! the tests (red first).

use std::sync::Arc;

use super::{Flaw, PostgresConfig, PostgresStore};
use crate::clock::ManualClock;
use crate::conformance::Backend;
use crate::{Conflict, Txn};

/// Opens stores on one Postgres database, one schema each.
pub struct PostgresBackend;

impl PostgresBackend {
    /// A backend on the database at `url`, whose stores carry `flaw`.
    #[must_use]
    pub fn new(_url: &str, _flaw: Flaw) -> PostgresBackend {
        PostgresBackend
    }

    /// A configuration for a new schema.
    #[must_use]
    pub fn fresh_config(&self) -> PostgresConfig {
        todo!("av2-15: PostgresBackend::fresh_config")
    }
}

impl Backend for PostgresBackend {
    type Store = PostgresStore;

    fn open(&self, _clock: Arc<ManualClock>) -> PostgresStore {
        todo!("av2-15: PostgresBackend::open")
    }

    fn restart(&self, _store: PostgresStore) -> PostgresStore {
        todo!("av2-15: PostgresBackend::restart")
    }

    fn commit_out_of_order(
        &self,
        _store: &PostgresStore,
        _first: Txn,
        _second: Txn,
        _between: &mut dyn FnMut(),
    ) -> Result<(), Conflict> {
        todo!("av2-15: PostgresBackend::commit_out_of_order")
    }
}
