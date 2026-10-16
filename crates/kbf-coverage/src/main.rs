//! `kbf-coverage`: prints per-crate line and branch coverage from an lcov tracefile and
//! checks it against the `coverage-baseline` ratchet (`check`, also the default when
//! no subcommand is named), or lowers that file to a measurement (`tighten`). See the
//! library docs.
//!
//! Exit status: 0 when every crate passes (`check`) or the file now matches the
//! measurement (`tighten`), 1 when the ratchet fails or `tighten` refuses, 2 when an
//! input cannot be read or parsed (and for usage errors, as clap reports them).

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, CommandFactory, Parser, Subcommand};
use kbf_coverage::baseline::{self, Baseline, Entry};
use kbf_coverage::{CrateCoverage, lcov, ratchet, tighten};

/// Per-crate coverage table and ratchet. With no subcommand, the flags are `check`'s.
#[derive(Debug, Parser)]
#[command(args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    check: Option<CheckArgs>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the table and check the ratchet (the default).
    Check(CheckArgs),
    /// Lower coverage-baseline to a measurement; refuses any rise.
    Tighten(TightenArgs),
}

#[derive(Debug, Args)]
struct CheckArgs {
    /// The lcov tracefile `cargo llvm-cov --lcov` wrote.
    #[arg(long)]
    lcov: PathBuf,
    /// The workspace root, spelled as the tracefile's source paths begin.
    #[arg(long)]
    root: PathBuf,
    /// The baseline file to check against.
    #[arg(long)]
    baseline: PathBuf,
    /// Also write the measured coverage, as a baseline file, to this path.
    #[arg(long)]
    write_baseline: Option<PathBuf>,
    /// Also append the table to this file (created if absent), as a CI job summary:
    /// `--summary-file "$GITHUB_STEP_SUMMARY"`.
    #[arg(long)]
    summary_file: Option<PathBuf>,
    /// The CI run that uploads `--write-baseline`'s file, so a failure `tighten` can
    /// fix prints the command that downloads it: `--run-id "$GITHUB_RUN_ID"`.
    #[arg(long, requires = "write_baseline")]
    run_id: Option<String>,
    /// The repository that run belongs to: `--repo "$GITHUB_REPOSITORY"`.
    #[arg(long, requires = "run_id")]
    repo: Option<String>,
    /// The name of the artifact that run uploads `--write-baseline`'s file in.
    #[arg(long, default_value = "coverage", requires = "run_id")]
    artifact: String,
}

#[derive(Debug, Args)]
struct TightenArgs {
    /// The measured baseline file the coverage job writes (`--write-baseline`) and
    /// uploads as `coverage-baseline.measured`.
    #[arg(long, required_unless_present = "lcov", conflicts_with = "lcov")]
    measured: Option<PathBuf>,
    /// Measure from this lcov tracefile instead (paths under `--root`).
    #[arg(long)]
    lcov: Option<PathBuf>,
    /// The workspace root: its `crates/*/Cargo.toml` are the workspace's crates.
    #[arg(long, default_value = ".")]
    root: PathBuf,
    /// The baseline file to lower, replaced whole [default: coverage-baseline under
    /// --root].
    #[arg(long)]
    baseline: Option<PathBuf>,
}

const DEFAULT_BASELINE: &str = "coverage-baseline";

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match (cli.command, cli.check) {
        (Some(Command::Check(args)), _) | (None, Some(args)) => check(&args),
        (Some(Command::Tighten(args)), _) => run_tighten(&args),
        (None, None) => Cli::command()
            .error(
                clap::error::ErrorKind::MissingRequiredArgument,
                "name a subcommand, or give `check`'s flags (--lcov, --root, --baseline)",
            )
            .exit(),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("kbf-coverage: {e}");
            ExitCode::from(2)
        }
    }
}

fn read(path: &Path) -> Result<String, Box<dyn Error>> {
    std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()).into())
}

fn append(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?
        .write_all(text.as_bytes())
}

fn read_baseline(path: &Path) -> Result<(String, Baseline), Box<dyn Error>> {
    let text = read(path)?;
    let parsed = baseline::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((text, parsed))
}

fn workspace(root: &Path) -> Result<BTreeSet<String>, Box<dyn Error>> {
    ratchet::workspace_crates(root)
        .map_err(|e| format!("list crates under {}: {e}", root.display()).into())
}

/// The workspace's crates and each one's coverage in the tracefile `lcov`.
fn measure(lcov: &Path, root: &Path) -> Result<BTreeMap<String, CrateCoverage>, Box<dyn Error>> {
    let files = lcov::parse(&read(lcov)?).map_err(|e| format!("{}: {e}", lcov.display()))?;
    Ok(ratchet::by_crate(root, &workspace(root)?, &files)?)
}

fn entries(measured: &BTreeMap<String, CrateCoverage>) -> Baseline {
    measured
        .iter()
        .map(|(k, c)| (k.clone(), Entry::of(*c)))
        .collect()
}

/// Prints the table (and appends it to `--summary-file`), and on a failure what
/// `tighten` would do about it; returns whether the ratchet passed.
fn check(args: &CheckArgs) -> Result<bool, Box<dyn Error>> {
    let measured = measure(&args.lcov, &args.root)?;
    let now = entries(&measured);
    // Written before the check, so a failing run still leaves the file to copy.
    if let Some(path) = &args.write_baseline {
        std::fs::write(path, baseline::render(&now))
            .map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    let (_, recorded) = read_baseline(&args.baseline)?;
    let rows = ratchet::rows(&measured, &recorded);
    let mut report = ratchet::render(&rows);
    let pass = !rows.iter().any(|r| r.status().fails());
    if !pass {
        report.push('\n');
        report.push_str(&tighten::check_advice(
            &recorded,
            &now,
            &tighten_command(args),
        ));
    }
    print!("{report}");
    if let Some(path) = &args.summary_file {
        append(path, &report).map_err(|e| format!("append to {}: {e}", path.display()))?;
    }
    Ok(pass)
}

/// Quotes `s` for a POSIX shell when it holds anything but plain path characters.
fn quote(s: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "-_./:=@+,".contains(c);
    if !s.is_empty() && s.chars().all(plain) {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

fn arg(p: &Path) -> String {
    quote(&p.to_string_lossy())
}

/// The command that runs `tighten` on what this check measured: from the CI run's
/// artifact when `--run-id` names it, else from the local files. The download goes
/// to a fresh directory each time (`gh run download` will not overwrite a file), so
/// the whole command can run again, and then changes nothing.
fn tighten_command(args: &CheckArgs) -> String {
    let mut tighten = "cargo run -p kbf-coverage -- tighten".to_owned();
    if args.baseline != Path::new(DEFAULT_BASELINE) {
        let _ = write!(tighten, " --baseline {}", arg(&args.baseline));
    }
    match (&args.run_id, &args.write_baseline) {
        (Some(id), Some(written)) => {
            let repo = args
                .repo
                .as_ref()
                .map_or_else(String::new, |r| format!(" --repo {}", quote(r)));
            // The artifact holds the file at its top level, under its own name.
            let name = written.file_name().unwrap_or(written.as_os_str());
            format!(
                "d=$(mktemp -d) && gh run download {}{repo} --name {} --dir \"$d\" && \
                 {tighten} --measured \"$d\"/{}",
                quote(id),
                quote(&args.artifact),
                arg(Path::new(name)),
            )
        }
        (_, Some(written)) => format!("{tighten} --measured {}", arg(written)),
        (_, None) => format!(
            "{tighten} --lcov {} --root {}",
            arg(&args.lcov),
            arg(&args.root)
        ),
    }
}

/// Lowers `--baseline` to the measurement; returns false (file untouched) on a refusal.
fn run_tighten(args: &TightenArgs) -> Result<bool, Box<dyn Error>> {
    let baseline_path = args
        .baseline
        .clone()
        .unwrap_or_else(|| args.root.join(DEFAULT_BASELINE));
    let baseline_path = baseline_path.as_path();
    let measured = match (&args.measured, &args.lcov) {
        (Some(path), _) => read_baseline(path)?.1,
        (None, lcov) => {
            // clap requires one of the two.
            let lcov = lcov.as_deref().unwrap_or(Path::new(""));
            entries(&measure(lcov, &args.root)?)
        }
    };
    let workspace = workspace(&args.root)?;
    let (text, recorded) = read_baseline(baseline_path)?;
    let changes = match tighten::plan(&recorded, &measured, &workspace) {
        Ok(changes) => changes,
        Err(refusals) => {
            eprintln!(
                "kbf-coverage tighten: refused; {} is unchanged. tighten only lowers counts:",
                baseline_path.display()
            );
            for r in &refusals {
                eprintln!("  {r}");
            }
            if let Some(advice) = tighten::tree_advice(&refusals, args.measured.is_some()) {
                eprintln!("{advice}");
            }
            return Ok(false);
        }
    };
    if changes.is_empty() {
        println!(
            "kbf-coverage tighten: {} already matches the measurement; nothing to change",
            baseline_path.display()
        );
        return Ok(true);
    }
    let new = tighten::apply(&recorded, &changes);
    tighten::replace(baseline_path, &baseline::rewrite(&text, &new))
        .map_err(|e| format!("write {}: {e}", baseline_path.display()))?;
    println!(
        "kbf-coverage tighten: lowered {} to the measurement:",
        baseline_path.display()
    );
    for c in &changes {
        println!("  {c}");
    }
    Ok(true)
}
