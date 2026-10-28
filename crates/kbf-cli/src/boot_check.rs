//! `kbf boot-check --mode=<mode>`: the schema step a unit runs before it starts kbf.
//!
//! The packaged units run `+/usr/bin/kbf boot-check --mode=<mode>` as the first
//! `ExecStartPre=`. For a mode that keeps its state in the database (frontend, all-in-one:
//! [`names::keeps_state_in_database`]) the state database is brought to the schema this kbf
//! needs before the unit starts it. The schema is this binary's own: the newest migration it
//! embeds ([`kbf_pack_index::schema::latest`]), the number `kbf --version --json` prints
//! as `state_schema`; nothing is run to ask for it. Any failure ends the check with exit
//! status 1, so the unit does not start on a database its binary cannot use. The other modes
//! keep no state in the database, and a host whose `installer.toml` names no database has
//! nothing to migrate: both do nothing and the check passes.
//!
//! Where the database is: the `database` key of `installer.toml`
//! ([`DEFAULT_INSTALLER_CONFIG`], or `--installer-config`), a `postgres://` URL
//! (`postgres:///kbf?host=/run/postgresql`). The file is world-readable, so a URL that carries
//! a password is refused; peer authentication needs none. Only that one key is read. No
//! message quotes the URL, and a server's refusal of the connection is reduced to its SQLSTATE.
//!
//! The migrations are the ones embedded in this build, expand steps only. The step waits at
//! most [`kbf_state::postgres::boot::LOCK_WAIT`] for any lock, and a busy schema is a refusal.
//! [`kbf_state::postgres::boot`] has the step; what is here is the verb around it.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Arg, Command};
use kbf_state::postgres::PostgresConfig;
use kbf_state::postgres::boot::BootStep;

use crate::names;

/// The word after `kbf` that runs the check instead of a mode. It is not in
/// [`names::MODES`], so the mode list stays exact.
pub const BOOT_CHECK: &str = "boot-check";

/// The `installer.toml` the check reads when `--installer-config` does not name one.
pub const DEFAULT_INSTALLER_CONFIG: &str = "/etc/kbf/installer.toml";

/// The line of a check that did nothing because the mode keeps no state in the database.
pub const NO_STATE: &str = "schema step skipped: this mode keeps no state in the database";

/// The line of a check that did nothing because `installer.toml` names no database.
pub const NO_DATABASE: &str = "schema step skipped: installer.toml names no database";

/// The flag that names the mode of the unit.
const MODE: &str = "mode";

/// The flag that names the `installer.toml`.
const INSTALLER_CONFIG: &str = "installer-config";

/// What runs before a unit starts, so the verb can be tested without a server.
pub trait SchemaStep {
    /// Brings the database to schema `wanted`. The text is for the log.
    ///
    /// # Errors
    /// One line saying why the unit must not start.
    fn before_start(&mut self, wanted: u32) -> Result<String, String>;
}

impl SchemaStep for BootStep {
    fn before_start(&mut self, wanted: u32) -> Result<String, String> {
        self.migrate_to(wanted)
    }
}

/// Brings the database to the schema this build needs, when `mode` keeps its state there and
/// `database` (asked for only then) has a step. The text is for the log: what the step did,
/// or why nothing was done.
///
/// # Errors
/// One line saying why the unit must not start: `database` failed, or the step refused.
pub fn migrate_before_start<S: SchemaStep>(
    mode: &str,
    database: impl FnOnce() -> Result<Option<S>, String>,
) -> Result<String, String> {
    if !names::keeps_state_in_database(mode) {
        return Ok(NO_STATE.to_owned());
    }
    let Some(mut step) = database()? else {
        return Ok(NO_DATABASE.to_owned());
    };
    step.before_start(kbf_pack_index::schema::latest())
        .map(|said| format!("schema step: {said}"))
        .map_err(|why| format!("state schema: {why}"))
}

/// The database step of the `installer.toml` at `path`; `None` when the file has no
/// `database` key (or it is not a string).
///
/// # Errors
/// One line saying why the database cannot be reached from the file: it cannot be read, is
/// not TOML, or its `database` URL is unusable or carries a password. Never the URL.
pub fn database_of(path: &Path) -> Result<Option<BootStep>, String> {
    let shown = path.display().to_string();
    let text = std::fs::read_to_string(path).map_err(|e| format!("{shown}: {}", e.kind()))?;
    database_in(&shown, &text)
}

/// [`database_of`] over text already read from the file named `shown`.
fn database_in(shown: &str, text: &str) -> Result<Option<BootStep>, String> {
    let table: toml::Table = text
        .parse()
        .map_err(|_| format!("{shown} is not valid TOML"))?;
    let Some(url) = table.get("database").and_then(toml::Value::as_str) else {
        return Ok(None);
    };
    let config = PostgresConfig::parse(url)
        .map_err(|why| format!("the `database` URL in {shown} is not usable: {why}"))?;
    if config.has_password() {
        return Err(format!(
            "the `database` URL in {shown} carries a password; use peer authentication"
        ));
    }
    Ok(Some(kbf_pack_index::schema::boot_step(config)))
}

/// A parsed `kbf boot-check` command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Args {
    /// The mode of the unit that runs the check.
    pub mode: String,
    /// The `installer.toml` whose `database` key is the state database.
    pub installer_config: PathBuf,
}

/// `kbf boot-check ...`'s command line.
#[must_use]
pub fn command() -> Command {
    Command::new(BOOT_CHECK)
        .bin_name(format!("kbf {BOOT_CHECK}"))
        .about(
            "Bring the state database to the schema this kbf needs, before a unit starts it; \
             exits 1, and the unit does not start, when it cannot",
        )
        .disable_help_subcommand(true)
        .arg(
            Arg::new(MODE)
                .long(MODE)
                .value_name("MODE")
                .required(true)
                .value_parser(crate::mode_values())
                .help(
                    "The mode of the unit that runs the check: only the frontend keeps \
                     state in the database, the others pass",
                ),
        )
        .arg(
            Arg::new(INSTALLER_CONFIG)
                .long(INSTALLER_CONFIG)
                .value_name("FILE")
                .value_parser(clap::value_parser!(PathBuf))
                .default_value(DEFAULT_INSTALLER_CONFIG)
                .help(
                    "The installer.toml whose `database` key is the state database \
                     (a postgres:// URL with no password)",
                ),
        )
}

/// Parses `kbf boot-check ...`; `args` is the whole command line, its first word the
/// program's name and its second [`BOOT_CHECK`].
///
/// # Errors
/// What `clap` refuses: no `--mode`, a word that is not a mode, any other flag or word.
pub fn parse(args: &[OsString]) -> Result<Args, clap::Error> {
    crate::refuse_all_in_one(args)?;
    let argv = args.iter().take(1).chain(args.iter().skip(2));
    let matches = command().try_get_matches_from(argv)?;
    Ok(Args {
        mode: matches
            .get_one::<String>(MODE)
            .cloned()
            .expect("--mode is required"),
        installer_config: matches
            .get_one::<PathBuf>(INSTALLER_CONFIG)
            .cloned()
            .expect("--installer-config has a default"),
    })
}

/// Runs the check: the line it says on standard error, and exit status 0, or 1 when the unit
/// must not start.
#[must_use]
pub fn run(args: &Args) -> ExitCode {
    let done = migrate_before_start(&args.mode, || database_of(&args.installer_config));
    report(&done, &mut std::io::stderr())
}

/// Says what the check came to on `err`, one line, and returns the exit status it stands for:
/// 0 for a line of what was done, 1 for the reason the unit must not start.
fn report(done: &Result<String, String>, err: &mut dyn Write) -> ExitCode {
    let (line, status) = match done {
        Ok(said) => (said, ExitCode::SUCCESS),
        Err(why) => (why, ExitCode::FAILURE),
    };
    // A closed pipe is the reader's business; the exit status still tells.
    let _ = writeln!(err, "kbf boot-check: {line}");
    status
}

#[cfg(test)]
#[path = "boot_check_tests.rs"]
mod tests;
