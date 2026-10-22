//! The command line: `export` today.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use kbf_server::api_client::OperatorClient;

use crate::export::{Export, Window, export};
use crate::readonly::{Api, ReadOnly, Transport};
use crate::replay::{Arm, Params, report, sim, trace};
use crate::row::Row;

#[derive(Debug, Parser)]
#[command(
    name = "kbf-estimator-replay",
    about = "Export the farm's run history and replay it"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Reads run records through the operator API (GET only) and writes one JSON line per
    /// attempt into one file per UTC day.
    Export(ExportArgs),
    /// Replays an export through the real scheduler, memory only, and prints the report.
    Replay(ReplayArgs),
}

#[derive(Debug, clap::Args)]
struct ReplayArgs {
    /// A directory of `runs-*.jsonl` files (an export), or one such file.
    #[arg(long)]
    from: PathBuf,
    /// Days that only teach the estimator.
    #[arg(long, default_value_t = 20)]
    train_days: u32,
    /// Days that are scored, after the training days.
    #[arg(long, default_value_t = 10)]
    score_days: u32,
    /// The memory ladder of the farm that made the export, in GiB.
    #[arg(long, value_delimiter = ',', default_values_t = [4, 16, 32])]
    ladder_gib: Vec<u64>,
    /// Also run the sample-lowering arm, which phase 1 does not include (an upper bound).
    #[arg(long)]
    with_upper_bound: bool,
}

#[derive(Debug, clap::Args)]
struct ExportArgs {
    /// The operator API's address.
    #[arg(long)]
    api: SocketAddr,
    /// A file holding the read-only credential (the bearer token).
    #[arg(long)]
    token_file: Option<PathBuf>,
    /// Attempts queued at or after this RFC 3339 time.
    #[arg(long)]
    since: String,
    /// Attempts queued before this RFC 3339 time.
    #[arg(long)]
    until: String,
    /// The directory the day files go to (created if missing).
    #[arg(long)]
    out: PathBuf,
}

/// The process entry: runs the verb the arguments name, prints its summary, and exits 0,
/// or prints the error to stderr and exits 1 (2 for arguments clap refuses).
pub fn main() -> ExitCode {
    let mut out = std::io::stdout();
    match run(std::env::args_os(), &mut out) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Usage(text)) => {
            eprintln!("{text}");
            ExitCode::from(2)
        }
        Err(Failure::Run(text)) => {
            eprintln!("kbf-estimator-replay: {text}");
            ExitCode::FAILURE
        }
    }
}

/// Why a run failed.
#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    /// The arguments are not valid (clap's text).
    Usage(String),
    /// The verb failed.
    Run(String),
}

/// Runs the verb `args` name, writing its summary to `out`.
///
/// # Errors
/// [`Failure`].
pub fn run<I, A>(args: I, out: &mut impl Write) -> Result<(), Failure>
where
    I: IntoIterator<Item = A>,
    A: Into<OsString> + Clone,
{
    let cli = Cli::try_parse_from(args).map_err(|e| Failure::Usage(e.to_string()))?;
    match cli.command {
        Command::Export(args) => {
            let token = args
                .token_file
                .as_deref()
                .map(OperatorClient::read_token)
                .transpose()
                .map_err(|e| Failure::Run(e.to_string()))?;
            let api = ReadOnly::new(Api::new(args.api, token));
            export_to(&api, &args, out).map_err(Failure::Run)
        }
        Command::Replay(args) => replay(&args, out).map_err(Failure::Run),
    }
}

fn replay(args: &ReplayArgs, out: &mut impl Write) -> Result<(), String> {
    let params = Params {
        ladder_gib: args.ladder_gib.clone(),
        train_days: args.train_days,
        score_days: args.score_days,
        ..Params::default()
    };
    let trace =
        trace::load(&args.from, &params).map_err(|e| format!("{}: {e}", args.from.display()))?;
    let mut arms = vec![Arm::Null, Arm::KeyFloor];
    if args.with_upper_bound {
        arms.push(Arm::SampleUpperBound);
    }
    let runs = arms
        .into_iter()
        .map(|arm| sim::simulate(&trace, arm, &params))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    out.write_all(report::render(&trace, &params, &runs).as_bytes())
        .map_err(|e| e.to_string())
}

fn export_to<T: Transport>(
    api: &ReadOnly<T>,
    args: &ExportArgs,
    out: &mut impl Write,
) -> Result<(), String> {
    let window = Window::parse(&args.since, &args.until).map_err(|e| e.to_string())?;
    let done = export(api, window).map_err(|e| e.to_string())?;
    let days =
        write_days(&done.rows, &args.out).map_err(|e| format!("{}: {e}", args.out.display()))?;
    summary(&done, &days, out).map_err(|e| e.to_string())
}

/// Writes `rows` (in order) to `dir/runs-YYYY-MM-DD.jsonl`, one file per queue day; returns
/// each day and its row count.
///
/// # Errors
/// A directory or file cannot be made or written.
pub fn write_days(rows: &[Row], dir: &Path) -> std::io::Result<BTreeMap<String, usize>> {
    fs::create_dir_all(dir)?;
    let mut by_day: BTreeMap<String, String> = BTreeMap::new();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for row in rows {
        let day = row.day();
        let text = by_day.entry(day.clone()).or_default();
        text.push_str(&row.line());
        text.push('\n');
        *counts.entry(day).or_default() += 1;
    }
    for (day, text) in by_day {
        fs::write(dir.join(format!("runs-{day}.jsonl")), text)?;
    }
    Ok(counts)
}

fn summary(
    done: &Export,
    days: &BTreeMap<String, usize>,
    out: &mut impl Write,
) -> std::io::Result<()> {
    let r = &done.report;
    writeln!(
        out,
        "invocations {} attempts {} rows {} without_run_id {} duplicate_run_ids {} run_gone {} outside_window {}",
        r.invocations,
        r.attempts,
        r.rows,
        r.without_run_id,
        r.duplicate_run_ids,
        r.run_gone,
        r.outside_window
    )?;
    for (day, n) in days {
        writeln!(out, "runs-{day}.jsonl {n}")?;
    }
    Ok(())
}
