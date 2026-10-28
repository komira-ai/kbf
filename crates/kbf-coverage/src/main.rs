//! `kbf-coverage`: prints per-crate line and branch coverage from an lcov tracefile and
//! checks it against the 99% gate and the `coverage-floors` file (`check`, also the
//! default when no subcommand is named), lifts floors to a measurement (`raise`), or
//! checks that a floors file lowers none of its base's (`no-lowering`), or adds the
//! tracefiles of sharded test runs into one (`merge`). See the library
//! docs.
//!
//! Exit status: 0 when every crate passes (`check`), the floors were written or need
//! no change (`raise`), or none was lowered (`no-lowering`); 1 when the gate fails or a
//! floor was lowered; 2 when an input cannot be read or parsed (and for usage errors,
//! as clap reports them).

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, CommandFactory, Parser, Subcommand};
use kbf_coverage::floors::{self, Floors};
use kbf_coverage::{CrateCoverage, gate, lcov, merge, raise, renames};

/// Per-crate coverage table and gate. With no subcommand, the flags are `check`'s.
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
    /// Print the table and check the gate (the default).
    Check(CheckArgs),
    /// Raise the floors in coverage-floors to a measurement; never adds or lowers one.
    Raise(RaiseArgs),
    /// Fail when a floors file has a lower threshold anywhere than its base's.
    NoLowering(NoLoweringArgs),
    /// Add the tracefiles of sharded test runs into the one a single run would write.
    Merge(MergeArgs),
}

#[derive(Debug, Args)]
struct MergeArgs {
    /// A directory holding one subdirectory per shard, each with an `lcov.info` (as
    /// `actions/download-artifact` lays out one artifact per shard).
    #[arg(long)]
    dir: PathBuf,
    /// How many shards there must be. Fewer or more is refused, so a shard whose
    /// tracefile never arrived cannot be left out of the total unnoticed.
    #[arg(long)]
    expect: usize,
    /// The merged tracefile to write.
    #[arg(long)]
    out: PathBuf,
}

#[derive(Debug, Args)]
struct CheckArgs {
    /// The lcov tracefile `cargo llvm-cov --lcov` wrote.
    #[arg(long)]
    lcov: PathBuf,
    /// The workspace root: absolute, spelled as the tracefile's source paths begin, or
    /// relative to the working directory.
    #[arg(long)]
    root: PathBuf,
    /// The floors file to check against.
    #[arg(long)]
    floors: PathBuf,
    /// Also append the table to this file (created if absent), as a CI job summary:
    /// `--summary-file "$GITHUB_STEP_SUMMARY"`.
    #[arg(long)]
    summary_file: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct RaiseArgs {
    /// The lcov tracefile to measure.
    #[arg(long)]
    lcov: PathBuf,
    /// The workspace root: its `crates/*/Cargo.toml` are the workspace's crates.
    #[arg(long, default_value = ".")]
    root: PathBuf,
    /// The floors file to raise, replaced whole [default: coverage-floors under
    /// --root].
    #[arg(long)]
    floors: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct NoLoweringArgs {
    /// The floors file of the pull request's base.
    #[arg(long)]
    base: PathBuf,
    /// The floors file of the pull request.
    #[arg(long)]
    floors: PathBuf,
    /// Pass, saying so, when `--base` does not exist (the base has no floors file yet);
    /// any other way of not reading it is still an error.
    #[arg(long)]
    allow_missing_base: bool,
    /// The git revision `--base` was taken from. A crate git sees renamed since it
    /// (`crates/A/Cargo.toml` to `crates/B/Cargo.toml`, with a new package name) keeps
    /// its base floor under the new name. Without it, no crate is renamed.
    #[arg(long)]
    base_ref: Option<String>,
}

const DEFAULT_FLOORS: &str = "coverage-floors";

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match (cli.command, cli.check) {
        (Some(Command::Check(args)), _) | (None, Some(args)) => check(&args),
        (Some(Command::Raise(args)), _) => run_raise(&args),
        (Some(Command::NoLowering(args)), _) => no_lowering(&args),
        (Some(Command::Merge(args)), _) => run_merge(&args),
        (None, None) => Cli::command()
            .error(
                clap::error::ErrorKind::MissingRequiredArgument,
                "name a subcommand, or give `check`'s flags (--lcov, --root, --floors)",
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

fn read_floors(path: &Path) -> Result<Floors, Box<dyn Error>> {
    floors::parse(&read(path)?).map_err(|e| format!("{}: {e}", path.display()).into())
}

fn workspace(root: &Path) -> Result<BTreeSet<String>, Box<dyn Error>> {
    gate::workspace_crates(root)
        .map_err(|e| format!("list crates under {}: {e}", root.display()).into())
}

/// The workspace's crates and each one's coverage in the tracefile `lcov`.
fn measure(lcov: &Path, root: &Path) -> Result<BTreeMap<String, CrateCoverage>, Box<dyn Error>> {
    let files = lcov::parse(&read(lcov)?).map_err(|e| format!("{}: {e}", lcov.display()))?;
    let sources = source_root(root)?;
    Ok(gate::by_crate(&sources, &workspace(root)?, &files)?)
}

/// `root` as the tracefile spells its source paths. `cargo llvm-cov` writes them
/// absolute, under the directory cargo found the workspace in (the process's
/// working directory, symbolic links resolved), so a relative `--root` (`.`, `..`)
/// is resolved the same way. An absolute one is taken as written, as CI passes it.
fn source_root(root: &Path) -> Result<PathBuf, Box<dyn Error>> {
    if root.is_absolute() {
        return Ok(root.to_path_buf());
    }
    std::fs::canonicalize(root)
        .map_err(|e| format!("resolve --root {}: {e}", root.display()).into())
}

/// Prints the table (and appends it to `--summary-file`), plus a note on floors that
/// could rise; returns whether the gate passed.
fn check(args: &CheckArgs) -> Result<bool, Box<dyn Error>> {
    let measured = measure(&args.lcov, &args.root)?;
    let recorded = read_floors(&args.floors)?;
    let rows = gate::rows(&measured, &recorded);
    let mut report = gate::render(&rows);
    let pass = !rows.iter().any(|r| r.status().fails());
    let command = format!(
        "cargo run -p kbf-coverage -- raise --lcov {} --root . --floors {}",
        args.lcov.display(),
        args.floors.display()
    );
    if let Some(advice) = raise::advice(&raise::plan(&recorded, &measured).1, &command) {
        report.push('\n');
        report.push_str(&advice);
    }
    print!("{report}");
    if let Some(path) = &args.summary_file {
        append(path, &report).map_err(|e| format!("append to {}: {e}", path.display()))?;
    }
    Ok(pass)
}

/// Raises `--floors` to the measurement.
fn run_raise(args: &RaiseArgs) -> Result<bool, Box<dyn Error>> {
    let path = args
        .floors
        .clone()
        .unwrap_or_else(|| args.root.join(DEFAULT_FLOORS));
    let measured = measure(&args.lcov, &args.root)?;
    let (raised, changes) = raise::plan(&read_floors(&path)?, &measured);
    if changes.is_empty() {
        println!(
            "kbf-coverage raise: {} already holds the highest floors; nothing to change",
            path.display()
        );
        return Ok(true);
    }
    raise::replace(&path, &floors::render(&raised))
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    println!("kbf-coverage raise: raised {}:", path.display());
    for c in &changes {
        println!("  {c}");
    }
    Ok(true)
}

/// Fails when `--floors` lowers a threshold of `--base`.
fn no_lowering(args: &NoLoweringArgs) -> Result<bool, Box<dyn Error>> {
    if args.allow_missing_base && !args.base.exists() {
        println!(
            "kbf-coverage no-lowering: {} does not exist, so the base has no floors to lower",
            args.base.display()
        );
        return Ok(true);
    }
    let renames = match &args.base_ref {
        Some(rev) => renames::git_renames(Path::new("."), rev)
            .map_err(|e| format!("--base-ref {rev}: {e}"))?,
        None => BTreeMap::new(),
    };
    let lowered = renames::lowered_renamed(
        &read_floors(&args.base)?,
        &read_floors(&args.floors)?,
        &renames,
    );
    if lowered.is_empty() {
        println!(
            "kbf-coverage no-lowering: {} lowers no threshold of {}",
            args.floors.display(),
            args.base.display()
        );
        return Ok(true);
    }
    eprintln!(
        "kbf-coverage no-lowering: {} lowers {} threshold(s) of {}:",
        args.floors.display(),
        lowered.len(),
        args.base.display()
    );
    for l in &lowered {
        eprintln!("  {l}");
    }
    Ok(false)
}

/// Adds the tracefiles under `--dir` into `--out`.
fn run_merge(args: &MergeArgs) -> Result<bool, Box<dyn Error>> {
    let entries =
        std::fs::read_dir(&args.dir).map_err(|e| format!("read {}: {e}", args.dir.display()))?;
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path().join("lcov.info"))
        .filter(|f| f.is_file())
        .collect();
    files.sort();
    if files.len() != args.expect {
        return Err(format!(
            "{} holds {} shard tracefile(s) (<shard>/lcov.info), expected {}",
            args.dir.display(),
            files.len(),
            args.expect
        )
        .into());
    }
    let mut inputs = Vec::new();
    for f in &files {
        inputs.push((f.display().to_string(), read(f)?));
    }
    let merged = merge::merge(&inputs)?;
    std::fs::write(&args.out, merged).map_err(|e| format!("write {}: {e}", args.out.display()))?;
    println!(
        "kbf-coverage merge: {} tracefile(s) into {}",
        files.len(),
        args.out.display()
    );
    Ok(true)
}
