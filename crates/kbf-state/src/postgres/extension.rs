//! What a crate that keeps tables of its own in the state schema needs from this one: pooled
//! connections to the same database and schema, opened under the same checks as the store's
//! (durable commits, a schema this build supports), and the store's error for a failed call.
//!
//! The crate brings its own migration ([`super::schema::Migration`], added to the set with
//! [`super::schema::extend`]) and its own statements. It gets no access to the store's tables or
//! statements, and its connections are its own: a call through them takes none of the locks
//! the store's transactions take.

use super::conn::Conn;
use super::session::{Pool, Pooled};
use super::{Fail, PostgresConfig, failed};
use crate::StoreError;

/// A small pool of connections to the database and schema a [`PostgresConfig`] names.
pub struct ExtensionPool {
    pool: Pool,
}

impl ExtensionPool {
    /// Connects once, with at most `config`'s connection limit in the pool, refusing a server
    /// that does not make commits durable and a schema this build does not support. It
    /// creates nothing: the installer owns the schema.
    ///
    /// # Errors
    /// [`StoreError::Failed`] when the server cannot be reached or refuses the schema.
    pub fn open(config: &PostgresConfig) -> Result<ExtensionPool, StoreError> {
        let pool = Pool::open(
            config.pg.clone(),
            config.schema.clone(),
            config.max_connections,
            config.supported,
        )
        .map_err(Fail::into_store)?;
        Ok(ExtensionPool { pool })
    }

    /// A connection on loan; it goes back to the pool on drop unless it was discarded.
    ///
    /// # Errors
    /// [`StoreError::Failed`] when a new connection was needed and could not be made.
    pub fn checkout(&self) -> Result<Loan<'_>, StoreError> {
        let loan = self.pool.checkout().map_err(Fail::into_store)?;
        Ok(Loan(loan))
    }
}

/// A connection on loan from an [`ExtensionPool`].
pub struct Loan<'p>(Pooled<'p>);

impl Loan<'_> {
    /// The connection. Its calls are safe inside a tokio runtime (they run off it).
    pub fn conn(&mut self) -> &mut Conn {
        &mut self.0.client
    }

    /// Drops the connection when the loan ends instead of pooling it: call it when the
    /// connection may be mid-transaction or otherwise in a state the next caller must not meet.
    pub fn discard(&mut self) {
        self.0.broken = true;
    }
}

/// A failed client call as the store reports it: what the server said, no connection URL.
#[must_use]
pub fn store_error(err: postgres::Error) -> StoreError {
    failed(err)
}
