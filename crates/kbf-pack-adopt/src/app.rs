//! One run of the tool: the checks, the adoption, the files it writes and the exit status.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Parser as _;
use kbf_pack::{AdoptReport, Adopter};
use kbf_pack_index::postgres::PostgresPackIndex;
use kbf_pack_index::{PackIndex, PackIndexError};
use kbf_state::postgres::PostgresConfig;
use serde::Serialize;

use crate::cli::Args;
use crate::error::Error;
use crate::summary;

/// How the pack index is opened from the `--pack-index` URL.
pub type OpenIndex = dyn Fn(&PostgresConfig) -> Result<Arc<dyn PackIndex>, PackIndexError>;

/// The places a test reaches into.
pub struct Seams {
    /// Opens the pack index. The tool opens it on Postgres ([`open_postgres`]); a test opens
    /// another one, or the same on a database whose URL a flag may not carry.
    pub open_index: Box<OpenIndex>,
}

impl Default for Seams {
    fn default() -> Self {
        Self {
            open_index: Box::new(open_postgres),
        }
    }
}

/// Opens the pack index on the Postgres state store `config` names, with connections of its
/// own. It creates nothing: the installer owns the tables.
///
/// # Errors
/// [`PackIndexError`]: the server cannot be reached, or the schema has no pack tables.
pub fn open_postgres(config: &PostgresConfig) -> Result<Arc<dyn PackIndex>, PackIndexError> {
    let index = PostgresPackIndex::open(config)?;
    Ok(Arc::new(index))
}

/// Runs the tool on a command line: prints the report to `out` and any error to `err`, and
/// returns the exit status (0 done, 2 refused or a usage error, 1 failed).
pub fn main_with(argv: Vec<OsString>, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    main_with_seams(argv, &Seams::default(), out, err)
}

/// [`main_with`] with the places a test reaches into passed in.
pub fn main_with_seams(
    argv: Vec<OsString>,
    seams: &Seams,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let args = match Args::try_parse_from(argv) {
        Ok(args) => args,
        Err(usage) => return usage_exit(&usage, out, err),
    };
    match run(&args, seams, out) {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(err, "kbf-pack-adopt: {error}");
            error.code()
        }
    }
}

/// Help goes to `out` and ends the run well; a usage error goes to `err` and refuses it.
fn usage_exit(usage: &clap::Error, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let text = usage.render().to_string();
    let failed = usage.use_stderr();
    let _ = if failed {
        err.write_all(text.as_bytes())
    } else {
        out.write_all(text.as_bytes())
    };
    u8::from(failed) * 2
}

/// The checks, the adoption and the files.
///
/// In order: the flags and the credentials file; the checkpoint file, if one is named and
/// exists; the pack index; the run, which saves the checkpoint as it goes; the report, printed
/// to `out` and, with `--report-json`, written to its file. The report is printed and written
/// whether the run ended well or not, and the file is written even when `out` cannot be.
///
/// # Errors
/// [`Error::Refused`] when a precondition does not hold (nothing was changed);
/// [`Error::Failed`] when a call fails; the run's own failure comes before a report file that
/// could not be written, and that before a report that could not be printed.
pub fn run(args: &Args, seams: &Seams, out: &mut dyn Write) -> Result<(), Error> {
    let plan = args.plan()?;
    let resume = load_checkpoint(plan.checkpoint_file.as_deref())?;
    let opened = (seams.open_index)(&plan.index);
    let index = opened.map_err(|error| Error::Failed(error.to_string()))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let adopter = Adopter::new(Arc::new(plan.store), index, plan.adopt);
    let checkpoint = plan.checkpoint_file.clone();
    let mut save = |report: &AdoptReport| -> io::Result<()> {
        match &checkpoint {
            Some(path) => write_json(path, report),
            None => Ok(()),
        }
    };
    let ended = runtime.block_on(adopter.run(resume, &mut save));
    let printed = out.write_all(summary::render(&ended.report).as_bytes());
    let written = match &plan.report_json {
        Some(path) => write_json(path, &ended.report),
        None => Ok(()),
    };
    if let Some(error) = ended.failure {
        return Err(error.into());
    }
    written?;
    printed?;
    Ok(())
}

/// The checkpoint in the file `--checkpoint-file` names: none if no file is named or none
/// exists yet.
fn load_checkpoint(path: Option<&Path>) -> Result<Option<AdoptReport>, Error> {
    let Some(path) = path else { return Ok(None) };
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(bad_checkpoint(path, &error.to_string())),
    };
    let parsed = serde_json::from_slice::<AdoptReport>(&bytes);
    let report = parsed.map_err(|error| {
        let why = format!("not a checkpoint: {error}");
        bad_checkpoint(path, &why)
    })?;
    Ok(Some(report))
}

fn bad_checkpoint(path: &Path, why: &str) -> Error {
    let shown = path.display();
    Error::refused(format!("--checkpoint-file {shown}: {why}"))
}

/// Writes `value` as JSON to `path`, through a file beside it and a rename, so that a reader
/// finds the old file or the new one and never half of either.
fn write_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    write_atomic(path, &bytes)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    let temp = PathBuf::from(name);
    let mut file = File::create(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&temp, path)
}
