//! The command line: `export`, `replay`, `slowdown` and `compare`.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use kbf_server::api_client::OperatorClient;

use crate::builds::{GroupBy, RunLabels};
use crate::compare;
use crate::export::{Export, Window, export};
use crate::readonly::{Api, ReadOnly, Transport};
use crate::replay::{Arm, Params, report, sim, trace};
use crate::row::Row;
use crate::slowdown;

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
    /// For each successful run, how much slower it was than the same key on a quiet host;
    /// prints a table, or JSON with every run.
    Slowdown(SlowdownArgs),
    /// Prints queue wait, wall, makespan and kills per 1,000 attempts for two exports, with
    /// per-build numbers (build action span (server): excludes the client's own analysis and
    /// local actions; queue time total; attempt queue wait) and the difference. Read-only.
    Compare(CompareArgs),
}

#[derive(Debug, clap::Args)]
struct CompareArgs {
    /// Arm A: a directory of `runs-*.jsonl` files (an export), or one such file.
    #[arg(long)]
    arm_a: PathBuf,
    /// Arm B, the same.
    #[arg(long)]
    arm_b: PathBuf,
    /// Print one JSON document instead of the table.
    #[arg(long)]
    json: bool,
    /// What a build is for the per-build numbers: one `tool_invocation_id` (one buck2
    /// command, what buck2's Build ID joins to) or one `correlated_invocations_id` (a CI
    /// job). The server keeps the first non-empty correlated id seen when an attempt ended,
    /// so `correlated` can split a build whose early calls sent none.
    #[arg(long, value_enum, default_value_t = GroupArg::Invocation)]
    group_by: GroupArg,
    /// With `--json`, the `setting` of every per-run record (`RunRecord`).
    #[arg(long)]
    setting: Option<String>,
    /// With `--json`, the `arm` of arm A's per-run records.
    #[arg(long)]
    label_a: Option<String>,
    /// With `--json`, the `arm` of arm B's per-run records.
    #[arg(long)]
    label_b: Option<String>,
    /// With `--json`, the `seed` of every per-run record.
    #[arg(long)]
    seed: Option<u64>,
    /// With `--json`, the `commit` of every per-run record.
    #[arg(long)]
    commit: Option<String>,
}

/// `--group-by`.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum GroupArg {
    Invocation,
    Correlated,
}

#[derive(Debug, clap::Args)]
struct SlowdownArgs {
    /// A directory of `runs-*.jsonl` files (an export), or one such file.
    #[arg(long)]
    from: PathBuf,
    /// A run is quiet if the node's CPU pressure at its start, in hundredths of a percent,
    /// was at most this.
    #[arg(long, default_value_t = slowdown::DEFAULT_QUIET_PRESSURE_CENTI)]
    quiet_pressure_centi: u64,
    /// ... and at most this many other exported attempts overlapped it on its node.
    #[arg(long, default_value_t = slowdown::DEFAULT_MAX_OVERLAP)]
    max_overlap: u64,
    /// The percentile (nearest rank) of a key's quiet walls that is its baseline.
    #[arg(long, default_value_t = slowdown::DEFAULT_BASELINE_PERCENTILE, value_parser = clap::value_parser!(u64).range(1..=100))]
    baseline_percentile: u64,
    /// Print one JSON document, with every run, instead of the table.
    #[arg(long)]
    json: bool,
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
        Command::Slowdown(args) => slowdown_to(&args, out).map_err(Failure::Run),
        Command::Compare(args) => compare_to(&args, out).map_err(Failure::Run),
    }
}

fn compare_to(args: &CompareArgs, out: &mut impl Write) -> Result<(), String> {
    let group = match args.group_by {
        GroupArg::Invocation => GroupBy::Invocation,
        GroupArg::Correlated => GroupBy::Correlated,
    };
    let arm = |path: &Path| {
        trace::read_rows(path)
            .map(|rows| compare::analyze_grouped(&rows, group))
            .map_err(|e| format!("{}: {e}", path.display()))
    };
    let (a, b) = (arm(&args.arm_a)?, arm(&args.arm_b)?);
    let given = [
        args.setting.is_some(),
        args.label_a.is_some(),
        args.label_b.is_some(),
        args.seed.is_some(),
        args.commit.is_some(),
    ];
    if !args.json && given.contains(&true) {
        return Err(
            "--setting, --label-a, --label-b, --seed and --commit label the per-run \
                    records of --json, and the table has none"
                .to_owned(),
        );
    }
    let text = if args.json {
        let labels = run_labels(args, &a, &b)?;
        compare::render_json_runs(&a, &b, labels.as_ref().map(|(la, lb)| (la, lb)))
    } else {
        format!(
            "A: {}\nB: {}\n{}",
            args.arm_a.display(),
            args.arm_b.display(),
            compare::render_text(&a, &b)
        )
    };
    out.write_all(text.as_bytes()).map_err(|e| e.to_string())
}

/// The labels of the two arms' per-run records, when either arm has builds to write; an
/// error when a label an arm with builds needs is missing, when one is empty, or when the
/// two arm labels are equal (js-analysis would get one run twice).
fn run_labels(
    args: &CompareArgs,
    a: &compare::ArmStats,
    b: &compare::ArmStats,
) -> Result<Option<(RunLabels, RunLabels)>, String> {
    let (need_a, need_b) = (a.builds.recorded(), b.builds.recorded());
    if ![need_a, need_b].contains(&true) {
        return Ok(None);
    }
    let missing: Vec<&str> = [
        ("--setting", true, args.setting.is_some()),
        ("--label-a", need_a, args.label_a.is_some()),
        ("--label-b", need_b, args.label_b.is_some()),
        ("--seed", true, args.seed.is_some()),
        ("--commit", true, args.commit.is_some()),
    ]
    .into_iter()
    .filter_map(|(flag, needed, given)| (needed && !given).then_some(flag))
    .collect();
    if !missing.is_empty() {
        return Err(format!(
            "--json writes one run record per build, and an export does not know their \
             labels: give {}",
            missing.join(", ")
        ));
    }
    let empty: Vec<&str> = [
        ("--setting", &args.setting),
        ("--label-a", &args.label_a),
        ("--label-b", &args.label_b),
        ("--commit", &args.commit),
    ]
    .into_iter()
    .filter_map(|(flag, value)| (value.as_deref() == Some("")).then_some(flag))
    .collect();
    if !empty.is_empty() {
        return Err(format!("{} must not be empty", empty.join(", ")));
    }
    if args.label_a.is_some() && args.label_a == args.label_b {
        return Err(
            "--label-a and --label-b are equal: two arms with one name would put the same \
             run twice into js-analysis"
                .to_owned(),
        );
    }
    let text = |v: &Option<String>| v.clone().unwrap_or_default();
    let labels = |arm: &Option<String>| RunLabels {
        setting: text(&args.setting),
        arm: text(arm),
        seed: args.seed.unwrap_or_default(),
        commit: text(&args.commit),
    };
    Ok(Some((labels(&args.label_a), labels(&args.label_b))))
}

fn slowdown_to(args: &SlowdownArgs, out: &mut impl Write) -> Result<(), String> {
    let rows = trace::read_rows(&args.from).map_err(|e| format!("{}: {e}", args.from.display()))?;
    let params = slowdown::Params {
        quiet_pressure_centi: args.quiet_pressure_centi,
        max_overlap: args.max_overlap,
        baseline_percentile: args.baseline_percentile,
    };
    let report = slowdown::analyze(&rows, &params);
    let text = if args.json {
        slowdown::render_json(&report)
    } else {
        slowdown::render_text(&report)
    };
    out.write_all(text.as_bytes()).map_err(|e| e.to_string())
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
