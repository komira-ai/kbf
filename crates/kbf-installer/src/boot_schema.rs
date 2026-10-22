//! The schema step of `boot-check` on a bundled host: one whose `kbf` is the package's own
//! file (`/usr/bin/kbf`), with no `releases/`, no `state.json` and so nothing for
//! [`crate::activate::boot_check`] to count or revert.
//!
//! The unit passes `boot-check --kbf=<path> --mode=<m>`; a connected host's unit passes
//! neither and keeps the check it has. With them, a unit of a mode that keeps its state in
//! the database (frontend, all-in-one) is started only after the database is at the schema
//! the file at `--kbf` declares: the file is asked (`--version --json`, the way the proxy's
//! migrator asks an installed release, but with no commit to compare with), and the
//! database step of `installer.toml` is run for what it says. Every failure ends the check
//! non-zero, so the unit does not start on a database its binary cannot use. The other modes
//! keep no state in the database, and a host whose `installer.toml` names none has nothing to
//! migrate: both do nothing and the check passes.
//!
//! The step waits at most [`DbMigrator::LOCK_WAIT`] for any lock, and a busy schema is a
//! refusal.

use std::path::Path;

use crate::cli::Mode;
use crate::database::{Bounded, Database, DatabaseError, SchemaStep};
use crate::proxy::migrator::{DbMigrator, probe_version, schema_of};
use crate::proxy::model::PortError;
use crate::run::Runner;

/// The line of a check that did nothing because the mode keeps no state in the database.
const NO_STATE: &str = "schema step skipped: this mode keeps no state in the database";

/// The line of a check that did nothing because `installer.toml` names no database.
const NO_DATABASE: &str = "schema step skipped: installer.toml names no database";

/// The database step of `installer.toml` at `path`, bounded by [`DbMigrator::LOCK_WAIT`];
/// `None` when the file has no `database` key.
///
/// # Errors
/// One line saying why the database cannot be reached from the file: it cannot be read, is
/// not TOML, or its `database` URL is unusable. Never the URL.
pub fn database_of(path: &Path) -> Result<Option<Bounded>, String> {
    match Database::from_installer_toml(path) {
        Ok(database) => Ok(Some(database.bounded(DbMigrator::LOCK_WAIT))),
        Err(DatabaseError::NoDatabase(_)) => Ok(None),
        Err(other) => Err(other.to_string()),
    }
}

/// Brings the database to the schema the file at `kbf` declares, when `mode` keeps its state
/// there and `database` (asked for only then) has a step. The text is for the log: what the
/// step did, or why nothing was done.
///
/// # Errors
/// One line saying why the unit must not start: `database` failed, `kbf` did not say its
/// schema (it cannot run, runs past the probe's bound, exits non-zero, prints something else
/// than a version object, or declares a schema that is not a `u32`), or the step refused.
pub fn migrate_before_start<S: SchemaStep>(
    runner: &dyn Runner,
    kbf: &Path,
    mode: Mode,
    database: impl FnOnce() -> Result<Option<S>, String>,
) -> Result<String, String> {
    if !mode.keeps_state_in_database() {
        return Ok(NO_STATE.to_owned());
    }
    let Some(mut step) = database()? else {
        return Ok(NO_DATABASE.to_owned());
    };
    let unread = |e: PortError| format!("{} did not say its schema: {}", kbf.display(), e.0);
    let version = probe_version(runner, kbf).map_err(unread)?;
    let schema = schema_of(&version).map_err(unread)?;
    let said = step
        .before_start(schema)
        .map_err(|why| format!("state schema: {why}"))?;
    Ok(format!("schema step: {said}"))
}

/// [`migrate_before_start`] for the host whose `installer.toml` is at `installer_toml`.
///
/// # Errors
/// As [`migrate_before_start`].
pub fn before_start(
    runner: &dyn Runner,
    installer_toml: &Path,
    kbf: &Path,
    mode: Mode,
) -> Result<String, String> {
    migrate_before_start(runner, kbf, mode, || database_of(installer_toml))
}

#[cfg(test)]
#[path = "boot_schema_tests.rs"]
mod tests;
