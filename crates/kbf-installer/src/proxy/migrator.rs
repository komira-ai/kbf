//! The real [`Migrator`]: brings the state database to the schema the staged release needs
//! before its candidate child starts (FP section 8).
//!
//! It adds nothing to the schema code. [`Database`] already owns the migrations (embedded
//! in this installer build), the `schema_version` table, the advisory lock, the one
//! transaction per migration and the expand-only lint; `kbf-installer setup` and the root
//! half's `activate` use the same. What is here is the swap's side of it:
//!
//! - **Which schema.** The `state_schema` of the staged release, read from `status.json`
//!   through [`RootOrders::staged`], and only when the staged commit is the one asked for.
//! - **A bounded wait.** The swap calls this with the old child serving (and, for an
//!   all-in-one host, its streams held), so every lock wait ends after
//!   [`DbMigrator::LOCK_WAIT`]: a second installer holding the schema lock, or a long read
//!   on a table a migration alters, is `database_busy`, not a hang.
//! - **A refusal word.** A refused migration is a [`PortError`] whose text is
//!   `<word>: <why>`; the swap records it as `failure.reason` of step `migrate`, so the
//!   word is what an operator or a script matches. The words are [`Refusal::word`] and do
//!   not change. The old child is never touched: a migration that fails rolled back its
//!   own transaction, and earlier ones (all additive) stay.
//! - **The credential.** The database is the one `installer.toml` names, as the role in
//!   that URL; the URL carries no password (`Database::from_installer_toml` refuses one),
//!   so there is no secret to leak, and no message here quotes the URL. A connection that
//!   fails says so in fixed words.

use std::time::Duration;

use kbf_state::postgres::schema::SchemaError;

use crate::CommitId;
use crate::database::Database;

use super::model::PortError;
use super::ports::{Migrator, RootOrders};

/// Why a migration was refused. Each has a stable [`word`](Refusal::word).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The release is not the staged one (or `status.json` cannot say).
    NotStaged,
    /// The release needs a schema this installer does not embed, or the database is ahead of
    /// it: update the kbf-installer package first.
    InstallerTooOld,
    /// A migration the release needs is not additive (a contract step, or an expand file that
    /// is not): an update never runs one.
    NotAdditive,
    /// The embedded set or the database's rows are inconsistent (numbering, a gap).
    SchemaInvalid,
    /// Another installer holds the schema lock, or a table is in use, past the wait.
    DatabaseBusy,
    /// A migration's statements failed and were rolled back.
    MigrationFailed,
    /// The server is unreachable or refused something outside a migration.
    DatabaseError,
}

impl Refusal {
    /// The word that leads the refusal's text. Never changes.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Refusal::NotStaged => "not_staged",
            Refusal::InstallerTooOld => "installer_too_old",
            Refusal::NotAdditive => "not_additive",
            Refusal::SchemaInvalid => "schema_invalid",
            Refusal::DatabaseBusy => "database_busy",
            Refusal::MigrationFailed => "migration_failed",
            Refusal::DatabaseError => "database_error",
        }
    }

    /// The refusal for what the schema code said. Every variant is named, so a new
    /// [`SchemaError`] is a compile error here and a decision, not a default.
    #[must_use]
    pub fn of(err: &SchemaError) -> Refusal {
        match err {
            SchemaError::NotShipped { .. } | SchemaError::DatabaseAhead { .. } => {
                Refusal::InstallerTooOld
            }
            SchemaError::ContractPending { .. }
            | SchemaError::Destructive { .. }
            | SchemaError::Forbidden { .. }
            | SchemaError::NoKind { .. } => Refusal::NotAdditive,
            SchemaError::Numbering { .. }
            | SchemaError::Gap { .. }
            | SchemaError::NotInitialised
            | SchemaError::TooOld { .. }
            | SchemaError::TooNew { .. } => Refusal::SchemaInvalid,
            SchemaError::Busy => Refusal::DatabaseBusy,
            SchemaError::Failed { .. } => Refusal::MigrationFailed,
            SchemaError::Database(_) => Refusal::DatabaseError,
        }
    }

    fn because(self, why: impl std::fmt::Display) -> PortError {
        PortError(format!("{}: {why}", self.word()))
    }
}

/// The swap's [`Migrator`] over the state database.
pub struct DbMigrator<'a> {
    db: Database,
    orders: &'a dyn RootOrders,
    lock_wait: Duration,
}

impl<'a> DbMigrator<'a> {
    /// The longest any lock is waited for: another installer's migration is seconds, and a
    /// swap in this step has nothing started yet.
    pub const LOCK_WAIT: Duration = Duration::from_secs(30);

    /// Migrates `db`, learning the schema a release needs from `orders`.
    #[must_use]
    pub fn new(db: Database, orders: &'a dyn RootOrders) -> DbMigrator<'a> {
        DbMigrator {
            db,
            orders,
            lock_wait: Self::LOCK_WAIT,
        }
    }

    /// Waits for a lock at most `wait` (a test shortens it).
    #[must_use]
    pub fn with_lock_wait(mut self, wait: Duration) -> DbMigrator<'a> {
        self.lock_wait = wait;
        self
    }

    /// The `state_schema` of `release`, if it is the staged one.
    fn needs(&self, release: &CommitId) -> Result<u32, PortError> {
        match self.orders.staged() {
            Ok(Some(facts)) if facts.commit == *release => Ok(facts.state_schema),
            Ok(_) => Err(Refusal::NotStaged.because("the staged release is not this update's")),
            Err(e) => Err(Refusal::NotStaged.because(format!("status.json: {e}"))),
        }
    }
}

impl Migrator for DbMigrator<'_> {
    fn migrate_forward(&self, release: &CommitId) -> Result<(), PortError> {
        let schema = self.needs(release)?;
        self.db
            .migrate_within(schema, self.lock_wait)
            .map(drop)
            .map_err(|e| Refusal::of(&e).because(e))
    }
}
