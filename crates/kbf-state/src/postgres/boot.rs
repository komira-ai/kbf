//! The schema step that runs before a unit starts kbf (`kbf boot-check`): brings the state
//! database to the schema this build needs, and refuses when it cannot.
//!
//! It adds nothing to [`schema`]: the migrations, the `schema_version` table, the advisory
//! lock, the one transaction per migration and the expand-only lint are those of
//! [`schema::apply_within`]. What is here is the unit's side of it:
//!
//! - **Which schema.** The newest the migrations of the build embed ([`schema::latest_of`] its
//!   set): the number `kbf --version --json` prints as `state_schema`.
//!   Nothing is probed; a build asks its own constants.
//! - **A bounded wait.** The step runs while systemd starts a unit, so each wait is bounded:
//!   a connect after [`schema::CONNECT_LIMIT`] and every lock after [`LOCK_WAIT`]. A busy
//!   schema is a refusal, not a wait without end. The whole step is bounded by the start
//!   timeout of the unit, not by the step.
//! - **A line for the log.** What the step did, or the one line that says why the unit must
//!   not start. Neither quotes the connection URL, and a server's refusal of a connection is
//!   reduced to its SQLSTATE ([`schema::SchemaError`]).

use std::time::Duration;

use super::PostgresConfig;
use super::schema::{self, Migration, Outcome, SchemaError};

/// The longest any lock is waited for: another migration is seconds, and the unit being
/// started has nothing running yet.
pub const LOCK_WAIT: Duration = Duration::from_secs(30);

/// The state database to bring forward, the migrations to do it with and the longest to wait.
#[derive(Debug)]
pub struct BootStep {
    config: PostgresConfig,
    migrations: Vec<Migration>,
    lock_wait: Duration,
}

impl BootStep {
    /// A step with the migrations a release embeds (this crate's, [`schema::embedded`], and the
    /// crates that extend them, [`schema::extend`]) and the longest to wait for a lock
    /// ([`LOCK_WAIT`]).
    #[must_use]
    pub fn new(
        config: PostgresConfig,
        migrations: Vec<Migration>,
        lock_wait: Duration,
    ) -> BootStep {
        BootStep {
            config,
            migrations,
            lock_wait,
        }
    }

    /// Brings the database to schema `wanted` with the migrations of the step, expand steps
    /// only, waiting at most the step's lock wait. A database already at `wanted`, or past it,
    /// is left alone: a schema above what the build embeds is the one a later release left
    /// behind, and whether this build may serve it is judged when the store opens (it may
    /// unless a `contract` step is above). The text is for the log.
    ///
    /// # Errors
    /// One line saying why the unit must not start: the migrations embed fewer than `wanted`
    /// (the package is older than the schema asked for), a pending migration is a contract
    /// step, the server is busy past the wait, or it cannot be reached or refused a
    /// migration.
    pub fn migrate_to(&self, wanted: u32) -> Result<String, String> {
        let done = schema::apply_within(&self.config, &self.migrations, wanted, self.lock_wait);
        said(done)
    }
}

/// The line for what [`schema::apply_within`] came to, in the words of a kbf package: the
/// schema errors that name the installer package are put as this build's.
fn said(done: Result<Outcome, SchemaError>) -> Result<String, String> {
    match done {
        Ok(done) => Ok(done.to_string()),
        Err(SchemaError::DatabaseAhead { found, newest }) => Ok(format!(
            "the schema is at version {found}, ahead of the {newest} this build embeds, \
             nothing to apply"
        )),
        Err(SchemaError::NotShipped { wanted, newest }) => Err(format!(
            "the unit needs state schema {wanted} and this kbf only embeds up to {newest}: \
             install the kbf release that matches the database"
        )),
        Err(other) => Err(other.to_string()),
    }
}

#[cfg(test)]
#[path = "boot_tests.rs"]
mod tests;
