//! The Postgres backend (feature `postgres`). Stub: the types and signatures the tests
//! are written against, with no behaviour yet (red first).

mod backend;

pub use backend::PostgresBackend;

use crate::clock::StoreTime;
use crate::store::Store;
use crate::{Changes, Commit, CommitStats, Conflict, Revision, StoreError, Txn, Versioned};

/// A deliberate defect, to show the suite can fail on a Postgres store.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Flaw {
    /// A correct store.
    #[default]
    None,
    /// C5: revisions come from a sequence, outside the transaction.
    SequenceRevisions,
    /// The claim waits for a row another transaction holds instead of skipping it.
    ClaimBlocks,
}

/// Why a connection string is not usable.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// No URL was given.
    #[error("the postgres state store needs a URL (postgres://...)")]
    Missing,
    /// The URL is not a `postgres://` or `postgresql://` URL.
    #[error("the state store URL must start with postgres:// or postgresql://")]
    NotPostgres,
    /// The URL names no host.
    #[error("the state store URL names no host")]
    NoHost,
    /// The URL asks for TLS, which this client does not speak.
    #[error("the state store URL asks for TLS (sslmode), which this client does not support")]
    TlsUnsupported,
    /// The URL does not parse.
    #[error("the state store URL is not valid: {0}")]
    Invalid(String),
    /// The schema name is not lower-case letters, digits and underscores.
    #[error("the state store schema {0:?} must be plain lower case")]
    BadSchema(String),
}

/// Where the store is and how it connects.
#[derive(Clone, Debug)]
pub struct PostgresConfig;

impl PostgresConfig {
    /// Reads a `postgres://` URL.
    ///
    /// # Errors
    /// [`ConfigError`] when the URL is not usable.
    pub fn parse(_url: &str) -> Result<PostgresConfig, ConfigError> {
        todo!("av2-15: PostgresConfig::parse")
    }

    /// Keeps the store's tables in `schema`.
    ///
    /// # Errors
    /// [`ConfigError::BadSchema`] when the name is not plain lower case.
    pub fn with_schema(self, _schema: &str) -> Result<PostgresConfig, ConfigError> {
        todo!("av2-15: PostgresConfig::with_schema")
    }

    /// The schema the store's tables are in.
    #[must_use]
    pub fn schema(&self) -> &str {
        todo!("av2-15: PostgresConfig::schema")
    }
}

/// A row claimed by [`PostgresStore::claim`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claimed {
    /// The key.
    pub key: String,
    /// The value as it was when claimed.
    pub was: Versioned,
    /// The commit of the transaction the caller built for it.
    pub commit: Commit,
}

/// The state store on Postgres.
pub struct PostgresStore;

impl PostgresStore {
    /// Connects and creates the tables.
    ///
    /// # Errors
    /// [`StoreError`] when the server cannot be reached or does not flush commits.
    pub fn open(_config: &PostgresConfig) -> Result<PostgresStore, StoreError> {
        todo!("av2-15: PostgresStore::open")
    }

    /// Claims up to `limit` live rows under `prefix`.
    ///
    /// # Errors
    /// [`StoreError`] when the database fails.
    pub fn claim(
        &self,
        _prefix: &str,
        _limit: usize,
        _make: impl FnMut(&str, &Versioned) -> Option<Txn>,
    ) -> Result<Vec<Claimed>, StoreError> {
        todo!("av2-15: PostgresStore::claim")
    }
}

impl Store for PostgresStore {
    fn apply_batch(&self, _batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        todo!("av2-15: PostgresStore::apply_batch")
    }

    fn get(&self, _key: &str) -> Result<Option<Versioned>, StoreError> {
        todo!("av2-15: PostgresStore::get")
    }

    fn scan_prefix(&self, _prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        todo!("av2-15: PostgresStore::scan_prefix")
    }

    fn changes(&self, _after: Revision, _limit: usize) -> Result<Changes, StoreError> {
        todo!("av2-15: PostgresStore::changes")
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        todo!("av2-15: PostgresStore::reap_expired")
    }

    fn commit_stats(&self) -> CommitStats {
        todo!("av2-15: PostgresStore::commit_stats")
    }

    fn now(&self) -> StoreTime {
        todo!("av2-15: PostgresStore::now")
    }
}
