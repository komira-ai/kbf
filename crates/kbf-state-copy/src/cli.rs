//! The command line: the copy, and the `cutover` and `rollback` commands around it.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::Parser;

use crate::error::Error;
use crate::setting::Place;

const EXIT_STATUS: &str = "Exit status: 0 done, 2 refused (nothing was changed), 1 failed.";

const ONE_PLACE: &str = "give exactly one of --config and --unit-file";

const COPY_HELP: &str = "Commands that go with the copy:\n  \
    kbf-state-copy cutover ...   copy, verify, and point the frontend's setting at Postgres\n  \
    kbf-state-copy rollback ...  put the setting back as the cutover found it\n\
    Run either with --help for its arguments.\n\n\
    Exit status: 0 done, 2 refused (nothing was changed), 1 failed.";

/// Copies a frontend's SQLite state file into an empty Postgres store.
///
/// Without --apply this is a dry run: it opens the file read-only, reports each key family
/// with its row count and a SHA-256 digest of its rows, checks the target, and writes
/// nothing. With --apply it copies everything in one transaction, reads it back inside the
/// transaction and again on a new connection after the commit, and compares counts and
/// digests per family; a mismatch before the commit rolls the copy back.
#[derive(Debug, Parser)]
#[command(name = "kbf-state-copy", version, after_help = COPY_HELP)]
pub struct Args {
    /// The frontend's SQLite state file. The frontend must be stopped.
    pub state_db: PathBuf,
    /// The empty Postgres store to copy into, as a postgres:// URL.
    pub postgres_url: String,
    /// Copy for real. Without it nothing is written.
    #[arg(long)]
    pub apply: bool,
}

/// Where the frontend reads its state-store setting: give exactly one of the two.
#[derive(Debug, Clone, clap::Args)]
pub struct PlaceArgs {
    /// The frontend's kbf.toml: the `state-store` key of its `[scheduler]` table is the
    /// setting.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// A systemd unit file, or a drop-in of the unit, whose command line carries
    /// `--state-store=URL`: that flag is the setting. Run `systemctl daemon-reload` after.
    #[arg(long, value_name = "FILE")]
    pub unit_file: Option<PathBuf>,
}

impl PlaceArgs {
    /// The setting the arguments name.
    ///
    /// # Errors
    /// [`Error::Failed`] (a mistake on the command line) when both or neither is given.
    pub fn resolve(&self) -> Result<Place, Error> {
        match (&self.config, &self.unit_file) {
            (Some(file), None) => Ok(Place::Config(file.clone())),
            (None, Some(file)) => Ok(Place::UnitFile(file.clone())),
            _ => Err(Error::Failed(ONE_PLACE.to_owned())),
        }
    }
}

/// Moves a stopped frontend from its SQLite state file to Postgres, in one command.
///
/// Without --apply this is a dry run: it finds the frontend's state-store setting, checks
/// that the frontend is stopped (its state file lock is free and, with --unit, systemd says
/// the unit is inactive), runs the copy's dry run, and changes no file. With --apply it
/// copies and verifies as `kbf-state-copy <state.db> <postgres-url> --apply` does and then
/// rewrites the one setting to the Postgres URL. The original file is kept byte for byte as
/// `<file>.pre-postgres` (the command refuses when that file exists), and the new one is
/// written to a temporary file, synced and renamed over the original. The SQLite file is
/// never changed.
#[derive(Debug, Parser)]
#[command(name = "kbf-state-copy cutover", after_help = EXIT_STATUS)]
pub struct CutoverArgs {
    /// The frontend's SQLite state file. The frontend must be stopped.
    pub state_db: PathBuf,
    /// The empty Postgres store to copy into, as a postgres:// URL.
    pub postgres_url: String,
    #[command(flatten)]
    pub place: PlaceArgs,
    /// The frontend's systemd unit: it must exist, and be `inactive` (or `failed`).
    #[arg(long, value_name = "NAME")]
    pub unit: Option<String>,
    /// The postgres:// URL to write into the setting, when the frontend reaches the database
    /// differently from this tool (a socket URL, say, where this tool goes over the
    /// network). Without it the setting gets the URL of the copy. It may carry no password:
    /// the frontend refuses one.
    #[arg(long, value_name = "URL")]
    pub frontend_url: Option<String>,
    /// Copy and switch for real. Without it no file is changed.
    #[arg(long)]
    pub apply: bool,
}

/// Puts the frontend's state-store setting back as `cutover` found it.
///
/// Restores `<file>.pre-postgres` over the file byte for byte (a rename, which is atomic,
/// so the backup is gone afterwards) and refuses when there is no backup. Without --apply
/// it only says what it would do. It takes the same --config or --unit-file as the
/// cutover did. The SQLite file is never changed. Writes the frontend made to Postgres
/// after the switch are not in the SQLite file.
#[derive(Debug, Parser)]
#[command(name = "kbf-state-copy rollback", after_help = EXIT_STATUS)]
pub struct RollbackArgs {
    #[command(flatten)]
    pub place: PlaceArgs,
    /// The frontend's systemd unit: it must exist, and be `inactive` (or `failed`).
    #[arg(long, value_name = "NAME")]
    pub unit: Option<String>,
    /// Restore for real. Without it no file is changed.
    #[arg(long)]
    pub apply: bool,
}

/// A parsed command line.
#[derive(Debug)]
pub enum Request {
    /// `kbf-state-copy <state.db> <postgres-url>`
    Copy(Args),
    /// `kbf-state-copy cutover ...`
    Cutover(CutoverArgs),
    /// `kbf-state-copy rollback ...`
    Rollback(RollbackArgs),
}

/// Parses a command line (its first element is the program). The first argument decides:
/// `cutover` and `rollback` are the commands, anything else is the copy's arguments, so a
/// state file by either name is written `./cutover`.
///
/// # Errors
/// What clap says of a mistake, or of `--help` and `--version`.
pub fn parse(argv: Vec<OsString>) -> Result<Request, clap::Error> {
    let word = argv.get(1).and_then(|word| word.to_str());
    match word.map(str::to_owned).as_deref() {
        Some("cutover") => cutover_args(argv),
        Some("rollback") => rollback_args(argv),
        _ => Args::try_parse_from(argv).map(Request::Copy),
    }
}

fn cutover_args(argv: Vec<OsString>) -> Result<Request, clap::Error> {
    let parsed = CutoverArgs::try_parse_from(command(argv, "cutover"));
    parsed.map(Request::Cutover)
}

fn rollback_args(argv: Vec<OsString>) -> Result<Request, clap::Error> {
    let parsed = RollbackArgs::try_parse_from(command(argv, "rollback"));
    parsed.map(Request::Rollback)
}

/// `argv` with the program and the command word replaced by the name a usage line shows.
fn command(argv: Vec<OsString>, word: &str) -> Vec<OsString> {
    let mut named = vec![OsString::from(format!("kbf-state-copy {word}"))];
    named.extend(argv.into_iter().skip(2));
    named
}
