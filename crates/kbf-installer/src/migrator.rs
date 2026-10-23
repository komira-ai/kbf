//! The database migrator: brings the state database to the schema the staged release needs
//! before its candidate child starts (FP section 8).
//!
//! It adds nothing to the schema code. [`Database`] already owns the migrations (embedded
//! in this installer build), the `schema_version` table, the advisory lock, the one
//! transaction per migration and the expand-only lint; `kbf-installer setup` and the root
//! half's `activate` use the same. What is here is the swap's side of it:
//!
//! - **Which schema.** The `state_schema` of the release asked for, and only when it is a
//!   release whose facts are known. The staged release's are in `status.json`
//!   ([`Facts::staged_schema`]); that is the swap's. The installed release (a cold start,
//!   after a reboot) has none there once the root half has restarted, so it is asked: its own
//!   `--version --json` ([`installed_schema`]), as the unprivileged user the proxy runs as.
//!   Any other commit is `not_staged`: a guess could migrate past what the release runs on.
//! - **A bounded wait.** The swap calls this with the old child serving (and, for an
//!   all-in-one host, its streams held), so every lock wait ends after
//!   [`DbMigrator::LOCK_WAIT`]: a second installer holding the schema lock, or a long read
//!   on a table a migration alters, is `database_busy`, not a hang.
//! - **A refusal word.** A refused migration is a [`MigrateError`] whose text is
//!   `<word>: <why>`; the swap records it as `failure.reason` of step `migrate`, so the
//!   word is what an operator or a script matches. The words are [`Refusal::word`] and do
//!   not change. The old child is never touched: a migration that fails rolled back its
//!   own transaction, and earlier ones (all additive) stay.
//! - **The credential.** The database is the one `installer.toml` names, as the role in
//!   that URL; the URL carries no password (`Database::from_installer_toml` refuses one),
//!   so there is no secret to leak, and no message here quotes the URL. A connection that
//!   fails says so in fixed words, and a server's refusal of the connection (`FATAL: role
//!   "x" does not exist`, a `pg_hba` rejection) is reduced to its SQLSTATE, because the
//!   server's text quotes the role, database and host: the refusal reads
//!   `database_error: ... (SQLSTATE 28000)`. Server text about a migration's own
//!   statements (a table that does not exist) is kept: it names the migration's objects.

use std::path::Path;
use std::time::Duration;

use kbf_state::postgres::schema::{Outcome, SchemaError};

use crate::CommitId;
use crate::binary::Version;
use crate::database::Database;
use crate::run::{Runner, Spawn};

/// A migration or a probe that failed. For a refusal the text is `<word>: <why>` with a
/// [`Refusal::word`]; a failure to read `status.json` carries the reader's own text.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct MigrateError(pub String);

/// The staged release the root half's `status.json` names, and the `state_schema` it declares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedSchema {
    pub commit: CommitId,
    pub state_schema: u32,
}

/// The two facts of the root half's `status.json` that a migration reads.
pub trait Facts {
    /// The staged release and the schema it declares, if a release is staged.
    ///
    /// # Errors
    /// `status.json` cannot be read.
    fn staged_schema(&self) -> Result<Option<StagedSchema>, MigrateError>;

    /// The commit `status.json` reports as installed, if it reports one.
    ///
    /// # Errors
    /// `status.json` cannot be read.
    fn installed_commit(&self) -> Result<Option<CommitId>, MigrateError>;
}

/// Why a migration was refused. Each has a stable [`word`](Refusal::word).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The release is not the staged one (or `status.json` cannot say).
    NotStaged,
    /// The installed release did not say which schema it needs (its `--version --json`
    /// failed, or is not that release's).
    SchemaUnknown,
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
            Refusal::SchemaUnknown => "schema_unknown",
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

    fn because(self, why: impl std::fmt::Display) -> MigrateError {
        MigrateError(format!("{}: {why}", self.word()))
    }
}

/// The swap's migration over the state database.
pub struct DbMigrator<'a> {
    db: Database,
    facts: Box<dyn Facts + 'a>,
    installed: Option<Asked<'a>>,
    lock_wait: Duration,
    apply: Apply<'a>,
}

/// How the `state_schema` of the installed release is learnt: from the release itself.
type Asked<'a> = Box<dyn Fn(&CommitId) -> Result<u32, MigrateError> + 'a>;

/// What runs the migrations: the database, the schema wanted and the lock wait. A seam so
/// that a test sees which bounds are handed over.
type Apply<'a> = Box<dyn Fn(&Database, u32, Duration) -> Result<Outcome, SchemaError> + 'a>;

impl<'a> DbMigrator<'a> {
    /// The longest any lock is waited for: another installer's migration is seconds, and a
    /// swap in this step has nothing started yet.
    pub const LOCK_WAIT: Duration = Duration::from_secs(30);

    /// Migrates `db`, learning the schema a staged release needs from `facts`. A release
    /// that is not the staged one is refused until [`knowing_installed`](Self::knowing_installed).
    #[must_use]
    pub fn new(db: Database, facts: impl Facts + 'a) -> DbMigrator<'a> {
        DbMigrator {
            db,
            facts: Box::new(facts),
            installed: None,
            lock_wait: Self::LOCK_WAIT,
            apply: Box::new(Database::migrate_within),
        }
    }

    /// Also migrates for the installed release (the cold start's), asking `ask` which
    /// schema it needs when `status.json` does not say.
    #[must_use]
    pub fn knowing_installed(
        mut self,
        ask: impl Fn(&CommitId) -> Result<u32, MigrateError> + 'a,
    ) -> DbMigrator<'a> {
        self.installed = Some(Box::new(ask));
        self
    }

    /// Runs `apply` instead of the database, to see what it is handed.
    pub(crate) fn with_apply(mut self, apply: Apply<'a>) -> DbMigrator<'a> {
        self.apply = apply;
        self
    }

    /// Waits for a lock at most `wait` (a test shortens it).
    #[must_use]
    pub fn with_lock_wait(mut self, wait: Duration) -> DbMigrator<'a> {
        self.lock_wait = wait;
        self
    }

    /// The `state_schema` of `release`: the staged one's from `status.json`, else the
    /// installed one's from itself.
    fn needs(&self, release: &CommitId) -> Result<u32, MigrateError> {
        match self.facts.staged_schema() {
            Ok(Some(facts)) if facts.commit == *release => return Ok(facts.state_schema),
            Ok(_) => {}
            Err(e) => return Err(Refusal::NotStaged.because(format!("status.json: {e}"))),
        }
        let installed = self.facts.installed_commit().ok().flatten().as_ref() == Some(release);
        match (&self.installed, installed) {
            (Some(ask), true) => ask(release),
            _ => Err(Refusal::NotStaged.because("the staged release is not this update's")),
        }
    }

    /// Applies every forward migration `release` needs: idempotent, so running it again
    /// after a crash changes nothing.
    ///
    /// # Errors
    /// A [`MigrateError`] whose text starts with a [`Refusal::word`] and a colon; it never
    /// carries the database URL.
    pub fn migrate(&self, release: &CommitId) -> Result<(), MigrateError> {
        let schema = self.needs(release)?;
        (self.apply)(&self.db, schema, self.lock_wait)
            .map(drop)
            .map_err(|e| Refusal::of(&e).because(e))
    }
}

/// How long a release gets to print its version: the installer's own probe bound.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// What the binary at `program` prints for `--version --json`: run as the caller with an
/// empty environment in `/`, and read as strictly as the installer read it when it installed
/// the release. Nothing says whose build it is: [`installed_schema`] adds that check, and a
/// host that holds one binary and no release (a bundled one) has no commit to compare with.
///
/// # Errors
/// [`Refusal::SchemaUnknown`]: the program cannot run, runs past [`PROBE_TIMEOUT`], exits
/// non-zero, or prints something else than a version object.
pub fn probe_version(runner: &dyn Runner, program: &Path) -> Result<Version, MigrateError> {
    let unknown = |why: String| Refusal::SchemaUnknown.because(why);
    let ran = runner
        .run(&Spawn {
            program: program.to_owned(),
            args: vec!["--version".into(), "--json".into()],
            env: Vec::new(),
            cwd: "/".into(),
            run_as: None,
            timeout: PROBE_TIMEOUT,
        })
        .map_err(|e| unknown(format!("the release did not run: {}", e.kind())))?;
    if ran.timed_out || ran.code != Some(0) {
        return Err(unknown(format!(
            "--version --json did not finish cleanly (exit {:?}, timed out: {})",
            ran.code, ran.timed_out
        )));
    }
    Version::parse(&ran.stdout).map_err(|fault| unknown(fault.to_string()))
}

/// The `state_schema` `version` declares, as the number the migrations are counted in.
///
/// # Errors
/// [`Refusal::SchemaUnknown`]: it does not fit a `u32`.
pub fn schema_of(version: &Version) -> Result<u32, MigrateError> {
    u32::try_from(version.state_schema)
        .map_err(|_| Refusal::SchemaUnknown.because("state_schema is too large"))
}

/// The `state_schema` the release at `program` (the executable of `release`) declares:
/// [`probe_version`] of it, required to be `release`'s own build.
///
/// # Errors
/// [`Refusal::SchemaUnknown`]: the program cannot run, runs past [`PROBE_TIMEOUT`], exits
/// non-zero, prints something else than a version object, or names another commit.
pub fn installed_schema(
    runner: &dyn Runner,
    program: &Path,
    release: &CommitId,
) -> Result<u32, MigrateError> {
    let version = probe_version(runner, program)?;
    version
        .check(release)
        .map_err(|fault| Refusal::SchemaUnknown.because(fault))?;
    schema_of(&version)
}
