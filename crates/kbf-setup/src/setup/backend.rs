//! What `setup` does through the rest of the installer: the state database. It is a trait with
//! one real implementation here, so the tests of `run` play the server and the implementation is
//! tested on its own.

use kbf_state::postgres::PostgresConfig;

use super::SetupError;
use crate::database::{Database, SchemaOps};

/// Where the state database is reached from.
pub trait Schemas {
    /// The schema runner for the database `url` names (validated: no password).
    ///
    /// # Errors
    /// The URL is not one the Postgres client takes.
    fn of(&self, url: &str) -> Result<Box<dyn SchemaOps>, SetupError>;
}

/// The real server, with the migrations embedded in this build.
pub struct Postgres;

impl Schemas for Postgres {
    fn of(&self, url: &str) -> Result<Box<dyn SchemaOps>, SetupError> {
        // The client's own messages may quote the URL; none is kept.
        let config = PostgresConfig::parse(url)
            .map_err(|_| SetupError::Database("the --database URL is not usable".to_owned()))?;
        Ok(Box::new(Database::new(
            config,
            kbf_pack_index::schema::embedded(),
        )))
    }
}

/// What `run` takes from outside the host.
pub struct Backend<'a> {
    pub schemas: &'a dyn Schemas,
}
