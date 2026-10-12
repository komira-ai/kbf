//! `cargo xtask <verb>`: kbf's CI logic, in Rust with tests, so each workflow step is one
//! command (the alias is in `.cargo/config.toml`).
//!
//! - `build-commit check <binary> <commit>` and `build-commit stamps <commit>`
//!   ([`build_commit`]): kbf-server names the commit it was built from.
//! - `budget-diff --base <rev>` ([`budget`]): the interpreter budget may only shrink; a
//!   row the working tree's budget file has and `<rev>`'s does not is refused. CI runs
//!   it on pull requests, with the base branch as `<rev>`.
//! - `fetch --url <url> --sha256 <hex> --out <path> [--extract <format> [--member <path>]]`
//!   ([`fetch`]): a pinned tool, downloaded over HTTPS and refused unless its SHA-256
//!   matches, before anything is unpacked or written.
//!
//! Exit status: 0 when the check passed, 1 when it failed (one reason per line on
//! stderr), 2 on a usage error.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use clap::{Parser, Subcommand};

pub mod budget;
pub mod build_commit;
pub mod fetch;

/// Why a verb failed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("not a commit: {0:?} (expected 12 to 40 lowercase hex digits)")]
    NotACommit(String),
    #[error("--version is {version:?}; expected it to end +{}", &commit[..12])]
    WrongCommit { version: String, commit: String },
    #[error("control: {version:?} also passed for {}", &other[..12])]
    ControlPassed { version: String, other: String },
    #[error("{what}: {why}")]
    Spawn { what: String, why: String },
    #[error("{0}")]
    Failed(String),
    #[error("{}", .0.join("\n"))]
    Refused(Vec<String>),
    #[error(transparent)]
    Fetch(#[from] fetch::Error),
    /// Flags that do not fit together (exit status 2, like a clap error).
    #[error("{0}")]
    Usage(String),
}

impl Error {
    /// The error for a program that could not be started, or a file not read: `what`
    /// and the system's reason.
    fn spawn(what: String) -> impl FnOnce(std::io::Error) -> Error {
        move |e| Error::Spawn {
            what,
            why: e.to_string(),
        }
    }
}

#[derive(Parser)]
#[command(name = "cargo xtask", about = "kbf's CI steps")]
struct Cli {
    /// The repository root (default: the workspace this xtask was built from).
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    #[command(subcommand)]
    verb: Verb,
}

#[derive(Subcommand)]
enum Verb {
    /// kbf-server's `--version` names the commit it was built from.
    #[command(subcommand)]
    BuildCommit(BuildCommit),
    /// Refuse a row the budget file gains against `--base`.
    BudgetDiff {
        /// The revision to compare with (on a pull request, the base branch).
        #[arg(long)]
        base: String,
    },
    /// Download a pinned file, check its SHA-256 and install it (mode 0755).
    Fetch {
        /// Where to download it from (HTTPS; redirects must stay on HTTPS).
        #[arg(long)]
        url: String,
        /// The SHA-256 of the download (before unpacking), 64 lowercase hex digits.
        #[arg(long)]
        sha256: fetch::Sha256,
        /// The file to install; its directory is created when missing.
        #[arg(long)]
        out: PathBuf,
        /// How the download is packed (default: it is the file).
        #[arg(long, value_enum)]
        extract: Option<fetch::Format>,
        /// The file to take out of a tar (`--extract tar.gz` or `tar.xz`): its path
        /// inside the archive.
        #[arg(long)]
        member: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum BuildCommit {
    /// `<binary> --version` names `<commit>`, and not the control commit.
    Check { binary: PathBuf, commit: String },
    /// A plain build, two override stamps and a malformed override, from the checkout.
    Stamps { commit: String },
}

/// The workspace this xtask was built from.
fn default_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The budget file as `rev` holds it, read with `git show` in `root`.
fn budget_at(root: &Path, rev: &str) -> Result<String, Error> {
    let spec = format!("{rev}:{}", budget::BUDGET);
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["show", &spec])
        .output()
        .map_err(Error::spawn("git".to_owned()))?;
    if !out.status.success() {
        return Err(Error::Failed(format!(
            "git show {spec}: {}",
            String::from_utf8_lossy(&out.stderr).trim_end()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `budget-diff --base <rev>`: the line it prints when nothing was added.
fn budget_diff(root: &Path, rev: &str) -> Result<String, Error> {
    let base = budget_at(root, rev)?;
    let path = root.join(budget::BUDGET);
    let head = std::fs::read_to_string(&path).map_err(Error::spawn(path.display().to_string()))?;
    let added = budget::new_rows(&base, &head).map_err(Error::Refused)?;
    if added.is_empty() {
        return Ok(format!("ok: {} adds no row to {rev}'s", budget::BUDGET));
    }
    Err(Error::Refused(
        added
            .iter()
            .map(|r| {
                format!(
                    "{}:{}: a new {} row for `{}`; the budget only shrinks: port it to \
                     Rust instead",
                    budget::BUDGET,
                    r.line,
                    r.section.header(),
                    r.key
                )
            })
            .collect(),
    ))
}

/// Runs the verb `cli` names: the lines to print when it passed.
fn run(cli: Cli) -> Result<Vec<String>, Error> {
    let root = cli.root.unwrap_or_else(default_root);
    match cli.verb {
        Verb::BuildCommit(BuildCommit::Check { binary, commit }) => {
            Ok(vec![build_commit::check(&binary, &commit)?])
        }
        Verb::BuildCommit(BuildCommit::Stamps { commit }) => {
            // Cargo sets CARGO to itself for the programs it runs.
            let cargo = std::env::var_os("CARGO").unwrap_or(OsString::from("cargo"));
            build_commit::stamps(&cargo, &root, &commit)
        }
        Verb::BudgetDiff { base } => Ok(vec![budget_diff(&root, &base)?]),
        Verb::Fetch {
            url,
            sha256,
            out,
            extract,
            member,
        } => {
            let unpack = fetch::Unpack::new(extract, member).map_err(Error::Usage)?;
            let roots = fetch::host_roots();
            Ok(vec![fetch::fetch(&url, &sha256, &unpack, &out, &roots)?])
        }
    }
}

/// The program: parses the arguments, runs the verb and prints its result.
pub fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(lines) => {
            for line in lines {
                println!("{line}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("xtask: {e}");
            match e {
                Error::Usage(_) => ExitCode::from(2),
                _ => ExitCode::FAILURE,
            }
        }
    }
}
