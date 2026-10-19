//! The state database the installer owns: it creates the schema at `setup` and brings it
//! forward before each new release starts. kbf only checks the version at its own start
//! (`kbf_state::postgres::schema`), so a database nobody set up is refused there with one
//! line.
//!
//! Where the database is: the `database` key of `installer.toml`, a `postgres://` URL
//! (`postgres:///kbf?host=/run/postgresql`). The file is world-readable, so a URL that
//! carries a password is refused; peer authentication needs none. This module reads that
//! one key and leaves the others to their own readers. No message here quotes the URL.
//!
//! The migrations are the ones embedded in this installer build. An update needs schema
//! `state_schema` (what the staged release's `--version --json` says); the installer runs
//! the embedded migrations up to it, expand steps only, and refuses the update when it
//! embeds fewer than the release needs (the package is older than the release) or when
//! the next step is a contract step.

use std::fs;
use std::path::Path;
use std::time::Duration;

use kbf_state::postgres::schema::{self, Migration, Outcome, SchemaError};
use kbf_state::postgres::{ConfigError, PostgresConfig};

/// Why the database cannot be reached from `installer.toml`.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum DatabaseError {
    /// The file could not be read.
    #[error("{path}: {why}")]
    Unreadable {
        /// The file.
        path: String,
        /// The operating system's reason.
        why: String,
    },
    /// The file is not TOML.
    #[error("{0} is not valid TOML")]
    NotToml(String),
    /// No `database` key, or not a string.
    #[error("{0} has no `database = \"postgres://...\"` setting")]
    NoDatabase(String),
    /// The URL is not usable.
    #[error("the `database` URL in {path} is not usable: {why}")]
    BadUrl {
        /// The file.
        path: String,
        /// Which rule it broke (never the URL).
        why: ConfigError,
    },
    /// The URL carries a password, in a world-readable file.
    #[error("the `database` URL in {0} carries a password; use peer authentication")]
    Password(String),
}

/// What runs before a release starts, so the installer's helper can be tested without a
/// server.
pub trait SchemaStep {
    /// Brings the database to what a release needs. The text is for the log.
    ///
    /// # Errors
    /// One line saying why the release must not start.
    fn before_start(&mut self, release_schema: u32) -> Result<String, String>;
}

/// The installer's view of the state database.
#[derive(Clone, Debug)]
pub struct Database {
    config: PostgresConfig,
    migrations: Vec<Migration>,
}

impl Database {
    /// The database `installer.toml` at `path` names, with this build's migrations. Reads only
    /// the `database` key, leniently; `crate::installer_toml` is the closed reader of the whole
    /// file and carries the key only so that it does not reject it.
    ///
    /// # Errors
    /// [`DatabaseError`]: the file, the key or the URL is wrong.
    pub fn from_installer_toml(path: &Path) -> Result<Database, DatabaseError> {
        let shown = path.display().to_string();
        let text = fs::read_to_string(path).map_err(|e| DatabaseError::Unreadable {
            path: shown.clone(),
            why: e.kind().to_string(),
        })?;
        let table: toml::Table = text
            .parse()
            .map_err(|_| DatabaseError::NotToml(shown.clone()))?;
        let Some(url) = table.get("database").and_then(toml::Value::as_str) else {
            return Err(DatabaseError::NoDatabase(shown));
        };
        let config = PostgresConfig::parse(url).map_err(|why| DatabaseError::BadUrl {
            path: shown.clone(),
            why,
        })?;
        if config.has_password() {
            return Err(DatabaseError::Password(shown));
        }
        Ok(Database::new(config, schema::embedded()))
    }

    /// A database with the given migrations (a test plays another release).
    #[must_use]
    pub fn new(config: PostgresConfig, migrations: Vec<Migration>) -> Database {
        Database { config, migrations }
    }

    /// What `setup` does: creates the schema on a fresh database, or finishes bringing
    /// one forward; a database already at this build's version is left alone.
    ///
    /// # Errors
    /// [`SchemaError`].
    pub fn setup(&self) -> Result<Outcome, SchemaError> {
        let newest = self.migrations.last().map_or(0, Migration::version);
        schema::apply(&self.config, &self.migrations, newest)
    }

    /// Brings the database to schema `state_schema` (what a staged release reports) with
    /// this build's migrations, waiting at most `lock_wait` for any lock and failing as
    /// [`SchemaError::Busy`] after it. The proxy's swap calls this with its candidate
    /// not yet started, so it must not wait for ever.
    ///
    /// # Errors
    /// [`SchemaError`]: the installer embeds fewer migrations than the release needs, a
    /// pending one is a contract step, the server is busy or refuses a migration.
    pub fn migrate_within(
        &self,
        state_schema: u32,
        lock_wait: Duration,
    ) -> Result<Outcome, SchemaError> {
        schema::apply_within(&self.config, &self.migrations, state_schema, lock_wait)
    }

    /// What `setup` would do, changing nothing: the database's version (`None` before the
    /// first `setup`) and this build's newest.
    ///
    /// # Errors
    /// [`SchemaError`] when the server cannot be asked.
    pub fn plan(&self) -> Result<(Option<u32>, u32), SchemaError> {
        let newest = self.migrations.last().map_or(0, Migration::version);
        plan_of(schema::current(&self.config), newest)
    }
}

/// The plan from what asking the server for its version said: no schema yet is a plan
/// (`None`), any other failure is an error.
fn plan_of(
    current: Result<u32, SchemaError>,
    newest: u32,
) -> Result<(Option<u32>, u32), SchemaError> {
    match current {
        Ok(v) => Ok((Some(v), newest)),
        Err(SchemaError::NotInitialised) => Ok((None, newest)),
        Err(other) => Err(other),
    }
}

/// The log line (or the one-line refusal) for what [`schema::apply`] did.
fn step_line(applied: Result<Outcome, SchemaError>) -> Result<String, String> {
    applied
        .map(|out| out.to_string())
        .map_err(|e| e.to_string())
}

impl SchemaStep for Database {
    fn before_start(&mut self, release_schema: u32) -> Result<String, String> {
        step_line(schema::apply(
            &self.config,
            &self.migrations,
            release_schema,
        ))
    }
}

#[cfg(test)]
#[path = "database_tests.rs"]
mod tests;
