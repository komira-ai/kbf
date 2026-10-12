//! `kbf-baseline`: what ran where, and how long it took, in one or more buck2 builds,
//! from the event logs buck2 wrote (see the library docs for the input).
//!
//! Prints a table; `--json FILE` also writes the report as JSON. Exit status: 0 when
//! every log was read (failed builds are data, not errors), 2 when a log cannot be
//! read (and for usage errors, as clap reports them).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use kbf_baseline::read;
use kbf_baseline::report::{self, Report};

/// Client-side build farm numbers from buck2 event logs.
#[derive(Debug, Parser)]
struct Args {
    /// Event logs, `*.json-lines` or `*.json-lines.gz` (`buck2 build --event-log FILE`,
    /// or `buck2 log show PATH > FILE.json-lines`). One build each.
    #[arg(required = true)]
    logs: Vec<PathBuf>,
    /// Also write the report as JSON to this file.
    #[arg(long)]
    json: Option<PathBuf>,
}

fn main() -> ExitCode {
    match run(&Args::parse()) {
        Ok(table) => {
            print!("{table}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("kbf-baseline: {e}");
            ExitCode::from(2)
        }
    }
}

fn run(args: &Args) -> Result<String, String> {
    let mut builds = Vec::new();
    for path in &args.logs {
        let build = read::read_path(path).map_err(|e| format!("{}: {e}", path.display()))?;
        builds.push((path.display().to_string(), build));
    }
    let report = Report::of(&builds);
    if let Some(path) = &args.json {
        write_json(path, &report).map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    Ok(report::render_table(&report))
}

fn write_json(path: &Path, report: &Report) -> std::io::Result<()> {
    let mut text = serde_json::to_vec_pretty(report)?;
    text.push(b'\n');
    std::fs::write(path, text)
}
