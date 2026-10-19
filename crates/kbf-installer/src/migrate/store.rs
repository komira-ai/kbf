//! The first crossing of a frontend or all-in-one host onto Postgres (docs/design/frontend-proxy.md
//! sections 9 and 11). The first update to a release with the new layout cannot be a swap: the
//! host stops, the `kbf` schema is created in the host's Postgres by the installer's own schema
//! step ([`crate::database`], the step `setup` runs), the units' `--state-store` is switched from
//! the SQLite file to that Postgres, and the host starts. **No state is copied**: the farm starts
//! on an empty database, and the SQLite file is left exactly as it is, which is what a rollback
//! returns to (a Postgres row is never copied back).
//!
//! Postgres, its `kbf` role and its `kbf` database are the host preparation's (`setup` creates
//! neither the role nor the database either); the migration creates none of them and refuses
//! when the database cannot be asked.
//! A node (a storage, daemon or storage-and-daemon host) keeps no server state, so it has no
//! [`StateStore`] and none of these steps.

use kbf_state::postgres::PostgresConfig;
use kbf_state::postgres::schema::{self, Outcome, SchemaError};

use super::error::MigrateError;
use super::units::UnitOutcome;
use crate::database::Database;

/// The placement of section 11: one Postgres per frontend host on its Unix socket, role and
/// database `kbf`, peer authentication (no password). It is the `database` of `installer.toml`
/// and the URL `kbf all-in-one` opens when `--state-store` is left out.
pub const LOCAL_POSTGRES: &str = "postgres:///kbf?host=/run/postgresql";

/// The state database as the crossing asks it. [`StateStore::at`] makes the real one (the schema
/// step of `setup`, applied to the newest migration this installer embeds); a test's fake
/// stands in for a server.
pub trait StateDatabase {
    /// The schema's version (`None` before the first `setup`) and the newest this installer
    /// embeds, changing nothing. The error says why the server, the role or the database
    /// cannot be reached.
    ///
    /// # Errors
    /// One line: why the database cannot be asked.
    fn plan(&self) -> Result<(Option<u32>, u32), String>;

    /// Creates the schema, or brings an older one forward; the line for the report.
    ///
    /// # Errors
    /// One line: why the schema step was refused.
    fn setup(&self) -> Result<String, String>;
}

/// The report line for what the schema step did, or the one-line reason it did not.
pub(super) fn applied(result: Result<Outcome, SchemaError>) -> Result<String, String> {
    result.map(|out| out.to_string()).map_err(|e| e.to_string())
}

/// The real state database: the installer's own [`Database`] (the migrations this build
/// embeds) at a URL, opened when it is asked, so that a URL the parser refuses is an error line
/// of the step and not a panic.
struct AtUrl(String);

impl AtUrl {
    fn database(&self) -> Result<Database, String> {
        let config = PostgresConfig::parse(&self.0).map_err(|e| e.to_string())?;
        Ok(Database::new(config, schema::embedded()))
    }
}

impl StateDatabase for AtUrl {
    fn plan(&self) -> Result<(Option<u32>, u32), String> {
        self.database()?.plan().map_err(|e| e.to_string())
    }

    fn setup(&self) -> Result<String, String> {
        applied(self.database()?.setup())
    }
}

/// What a host that keeps its state in Postgres moves to: the URL its units' `--state-store`
/// is given, and the database the schema step is applied to.
pub struct StateStore {
    pub url: String,
    pub database: Box<dyn StateDatabase>,
}

impl StateStore {
    /// The host's own Postgres ([`LOCAL_POSTGRES`]), asked through the installer's schema step.
    #[must_use]
    pub fn local() -> StateStore {
        StateStore::at(LOCAL_POSTGRES)
    }

    /// The Postgres at `url`, asked through the installer's schema step.
    pub(super) fn at(url: &str) -> StateStore {
        StateStore {
            url: url.to_owned(),
            database: Box::new(AtUrl(url.to_owned())),
        }
    }

    /// Asks the database what the schema step would do, before anything on the host is
    /// changed. The refusal is a database that cannot be asked (Postgres, the `kbf` role or the
    /// `kbf` database is absent: they are not made here), or one ahead of this installer.
    pub(super) fn look(&self) -> Result<String, MigrateError> {
        let (found, newest) = self
            .database
            .plan()
            .map_err(MigrateError::NoStateDatabase)?;
        match found {
            None => Ok(format!(
                "no schema yet: the schema step creates it at version {newest}"
            )),
            Some(v) if v < newest => Ok(format!(
                "the schema is at version {v}: the schema step brings it to {newest}"
            )),
            Some(v) if v == newest => Ok(format!("the schema is at version {v}: nothing to apply")),
            Some(v) => Err(MigrateError::StateSchema(format!(
                "the schema is at version {v}, past the {newest} this kbf-installer embeds: \
                 update the kbf-installer package first"
            ))),
        }
    }
}

/// The warning every plan and every run of a host with a state store carries.
pub(super) const FRESH_START: &str = "the state store starts empty on Postgres and no state is copied from the SQLite file \
     (it is left as it is, and a rollback restarts on it): nodes enroll again, and the \
     cordons, `desired`, the release index, the run history and the builds in flight are not \
     carried over, so drain the farm first";

/// What the report says of one unit's `--state-store` after the crossing.
pub(super) fn describe(outcome: UnitOutcome, url: &str) -> String {
    match outcome {
        UnitOutcome::Rewritten => format!(
            "--state-store now names {url}; the SQLite file is left as it is (the original unit is kept)"
        ),
        UnitOutcome::Already => "--state-store already names a Postgres store".to_owned(),
        UnitOutcome::NotOurs => "gives no --state-store to switch: left alone".to_owned(),
        UnitOutcome::Missing => "no such unit file: skipped".to_owned(),
    }
}
