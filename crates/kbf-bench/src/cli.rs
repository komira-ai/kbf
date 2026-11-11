//! The `kbf-bench` command line, and the run it describes.

use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use clap::{Parser, Subcommand, ValueEnum};

use crate::bench::{self, Method, Options};
use crate::connect::{self, ConnectError, Target, Tls};
use crate::noop::{self, Arm, NoopError, Plan, Summary};
use crate::program::Arch;
use crate::report::Report;
use crate::tree::{self, Shape, TreeError};

/// Times blob uploads to (and reads from) a kbf-server's CAS, and no-op actions.
#[derive(Debug, Parser)]
#[command(name = "kbf-bench", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Sends a tree's blobs, each distinct digest once.
    Upload {
        #[command(flatten)]
        run: RunArgs,
        /// Ask FindMissingBlobs about every digest first, and send only the missing
        /// ones. The daemon does not ask for a whole tree first; its engine asks about
        /// each blob before sending it, as `bytestream` does either way.
        #[arg(long, value_enum, default_value_t = OnOff::Off)]
        find_missing: OnOff,
    },
    /// Reads the blobs of the tree an upload with the same tree flags and `--seed`
    /// sent, each checked against its digest.
    Read {
        #[command(flatten)]
        run: RunArgs,
    },
    /// Times Executes of actions that do nothing, from sending each to its done
    /// operation, against one or more frontends (the arms), one at a time and then many
    /// at once, and writes one JSON line per run.
    Noop(NoopArgs),
}

/// The flags of `noop`.
#[derive(Debug, clap::Args)]
pub struct NoopArgs {
    /// An arm: a name for the records and its frontend's REAPI listener (`https://`, or
    /// `http://` when it serves plain text), as `NAME=URL`. Give it once per arm; the
    /// arms' blocks alternate in the order given.
    #[arg(long = "arm", required = true, value_parser = parse_arm)]
    pub arms: Vec<(String, String)>,
    /// The TLS flags, the same for every arm.
    #[command(flatten)]
    pub tls: Tls,
    /// The REAPI instance name of every request.
    #[arg(long, default_value = "")]
    pub instance_name: String,
    /// A platform property of the action, as `NAME=VALUE`; repeat for more.
    #[arg(long = "platform", value_parser = parse_property)]
    pub platform: Vec<(String, String)>,
    /// The machine `./noop` is written for; the one kbf-bench runs on when left out.
    #[arg(long, value_enum)]
    pub arch: Option<Arch>,
    /// In every action's environment, so a new seed makes new actions; from the clock
    /// when left out. The table names it.
    #[arg(long)]
    pub seed: Option<u64>,
    /// Runs per arm one at a time.
    #[arg(long, default_value_t = 300)]
    pub serial_runs: u64,
    /// Runs per arm after those, `--in-flight` at a time.
    #[arg(long, default_value_t = 300)]
    pub parallel_runs: u64,
    /// The Executes open at once in the parallel runs, over all arms together.
    #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(u64).range(1..))]
    pub in_flight: u64,
    /// Runs per arm in a block: the arms' blocks alternate.
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u64).range(1..))]
    pub block: u64,
    /// The file the runs are written to, one JSON line each.
    #[arg(long)]
    pub jsonl: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum OnOff {
    On,
    Off,
}

/// The flags both commands take.
#[derive(Debug, clap::Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub target: Target,
    #[command(flatten)]
    pub tree: TreeArgs,
    /// How the blobs travel: `bytestream` (one Write or Read per blob, as the daemon
    /// does) or `batch` (BatchUpdateBlobs or BatchReadBlobs of up to `--batch-bytes`).
    #[arg(long, value_enum, default_value_t = Method::Bytestream)]
    pub method: Method,
    /// The most requests in flight at once, and the engine's limit (the daemon's
    /// `--transfer-in-flight`, default 64).
    #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(u64).range(1..))]
    pub in_flight: u64,
    /// The most blob data one batch call carries (the front allows 16 MiB); a larger
    /// blob goes by ByteStream.
    #[arg(long, default_value = "4MiB", value_parser = parse_size)]
    pub batch_bytes: u64,
    /// Stop after this many seconds and report what finished; 0 waits for all.
    #[arg(long, default_value_t = 0)]
    pub timeout_secs: u64,
    /// Print a progress line to stderr this often; 0 prints none.
    #[arg(long, default_value_t = 10)]
    pub progress_secs: u64,
    /// Also write the report as JSON to this file.
    #[arg(long)]
    pub json: Option<PathBuf>,
}

/// The tree: generated (the defaults are the shape of one measured output tree) or
/// read from a directory.
#[derive(Debug, clap::Args)]
pub struct TreeArgs {
    /// Files to generate, the empty ones included.
    #[arg(long, default_value_t = 15_000)]
    pub files: u64,
    /// Empty files among them.
    #[arg(long, default_value_t = 144)]
    pub empty: u64,
    /// Directories to spread them over.
    #[arg(long, default_value_t = 989)]
    pub dirs: u64,
    /// The bytes of all files together: digits, then B, KB, MB, GB, KiB, MiB or GiB.
    #[arg(long, default_value = "307MB", value_parser = parse_size)]
    pub total_bytes: u64,
    /// Send this directory's files instead (each non-empty one with 16 bytes of the
    /// seed appended).
    #[arg(long, conflicts_with_all = ["files", "empty", "dirs", "total_bytes"])]
    pub from_dir: Option<PathBuf>,
    /// What the contents are drawn from (or salted with); the report names it. Upload
    /// picks one from the clock when it is left out; read needs the upload's.
    #[arg(long)]
    pub seed: Option<u64>,
}

/// Why a run did not start, or its report was not written.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("{0}")]
    Usage(&'static str),
    #[error(transparent)]
    Tree(#[from] TreeError),
    #[error(transparent)]
    Connect(#[from] ConnectError),
    #[error("--arm {0}: given twice")]
    TwiceNamed(String),
    #[error("--arm {arm}: {source}")]
    Arm {
        arm: String,
        #[source]
        source: ConnectError,
    },
    #[error(transparent)]
    Noop(#[from] NoopError),
    #[error("--jsonl {path}: {source}")]
    Jsonl {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("--json {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

const UNITS: [(&str, u64); 7] = [
    ("GiB", 1 << 30),
    ("MiB", 1 << 20),
    ("KiB", 1 << 10),
    ("GB", 1_000_000_000),
    ("MB", 1_000_000),
    ("KB", 1_000),
    ("B", 1),
];

/// A size: digits, then nothing, B, KB, MB, GB (powers of 1000), KiB, MiB or GiB
/// (powers of 1024).
///
/// # Errors
/// Anything else, or a size past `u64`.
pub fn parse_size(text: &str) -> Result<u64, String> {
    let (digits, scale) = UNITS
        .iter()
        .find_map(|&(unit, scale)| text.strip_suffix(unit).map(|digits| (digits, scale)))
        .unwrap_or((text, 1));
    let size = digits
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(scale));
    size.ok_or_else(|| {
        format!("{text:?} is not a size: digits, then B, KB, MB, GB, KiB, MiB or GiB")
    })
}

/// `NAME=URL`: a name of letters, digits, `-` and `_`, and a URL.
///
/// # Errors
/// No `=`, an empty or other name, or an empty URL.
pub fn parse_arm(text: &str) -> Result<(String, String), String> {
    let pair = text.split_once('=').filter(|(name, url)| {
        let named = name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() | (b == b'-') | (b == b'_'));
        named & !name.is_empty() & !url.is_empty()
    });
    pair.map(|(name, url)| (name.to_owned(), url.to_owned()))
        .ok_or_else(|| format!("{text:?} is not NAME=URL (a name of letters, digits, - and _)"))
}

/// `NAME=VALUE`, a platform property: a name that is not empty, and any value.
///
/// # Errors
/// No `=`, or an empty name.
pub fn parse_property(text: &str) -> Result<(String, String), String> {
    let pair = text.split_once('=').filter(|(name, _)| !name.is_empty());
    pair.map(|(name, value)| (name.to_owned(), value.to_owned()))
        .ok_or_else(|| format!("{text:?} is not NAME=VALUE"))
}

/// What a run of `kbf-bench` ended with.
#[derive(Clone, Debug, PartialEq)]
pub enum Finished {
    /// `upload` or `read`.
    Blobs(Report),
    Noop(Summary),
}

impl Finished {
    /// Everything planned succeeded.
    #[must_use]
    pub fn complete(&self) -> bool {
        match self {
            Self::Blobs(report) => report.complete(),
            Self::Noop(summary) => summary.complete(),
        }
    }

    /// The table for a terminal.
    #[must_use]
    pub fn table(&self) -> String {
        match self {
            Self::Blobs(report) => report.table(),
            Self::Noop(summary) => summary.table(),
        }
    }
}

/// Makes the run `cli` describes; returns what it ended with, complete or not.
///
/// # Errors
/// As [`blobs`] and [`noop`].
pub async fn run(cli: &Cli) -> Result<Finished, CliError> {
    match &cli.command {
        Command::Upload { run, find_missing } => blobs(run, *find_missing == OnOff::On, true).await,
        Command::Read { run } => blobs(run, false, false).await,
        Command::Noop(args) => noop(args).await.map(Finished::Noop),
    }
}

/// Connects to every arm and runs the benchmark `args` describes, writing its records to
/// `--jsonl`.
///
/// # Errors
/// An arm named twice; an arm's connection cannot be set up; the `--jsonl` file cannot
/// be made or written; an arm refused the actions' blobs.
pub async fn noop(args: &NoopArgs) -> Result<Summary, CliError> {
    let mut arms: Vec<Arm> = Vec::new();
    for (name, url) in &args.arms {
        if arms.iter().any(|arm| arm.name == *name) {
            return Err(CliError::TwiceNamed(name.clone()));
        }
        let channel =
            connect::channel_to(url.clone(), &args.tls).map_err(|source| CliError::Arm {
                arm: name.clone(),
                source,
            })?;
        arms.push(Arm {
            name: name.clone(),
            channel,
        });
    }
    let plan = Plan {
        instance_name: args.instance_name.clone(),
        platform: args.platform.clone(),
        arch: args
            .arch
            .or_else(|| Arch::from_name(std::env::consts::ARCH))
            .ok_or(CliError::Usage(
                "./noop is written for x86_64 and aarch64 only: name one with --arch",
            ))?,
        seed: args.seed.unwrap_or_else(clock_seed),
        serial_runs: args.serial_runs,
        parallel_runs: args.parallel_runs,
        in_flight: args.in_flight as usize,
        block: args.block,
    };
    let file = std::fs::File::create(&args.jsonl).map_err(|source| CliError::Jsonl {
        path: args.jsonl.clone(),
        source,
    })?;
    Ok(noop::run(&plan, &arms, &mut BufWriter::new(file)).await?)
}

/// Builds the tree, makes the `upload` (or `read`) `args` describes and writes its JSON;
/// returns the report, complete or not.
///
/// # Errors
/// `read` without `--seed`; the tree cannot be made; the connection cannot be set up;
/// the JSON cannot be written.
async fn blobs(args: &RunArgs, find_missing: bool, upload: bool) -> Result<Finished, CliError> {
    let seed = match (args.tree.seed, upload) {
        (Some(seed), _) => seed,
        (None, true) => clock_seed(),
        (None, false) => {
            return Err(CliError::Usage(
                "read needs the --seed the upload's report names",
            ));
        }
    };
    let tree = match &args.tree.from_dir {
        Some(dir) => tree::read_dir(dir, seed)?,
        None => {
            let shape = Shape {
                files: args.tree.files,
                empty: args.tree.empty,
                dirs: args.tree.dirs,
                total_bytes: args.tree.total_bytes,
            };
            tree::generate(&shape, seed)?
        }
    };
    let channel = connect::channel(&args.target)?;
    let seconds = |s: u64| (s > 0).then(|| Duration::from_secs(s));
    let options = Options {
        method: args.method,
        in_flight: args.in_flight as usize,
        find_missing,
        batch_bytes: args.batch_bytes,
        timeout: seconds(args.timeout_secs),
        progress: seconds(args.progress_secs),
    };
    let report = if upload {
        bench::upload(channel, &tree, options).await
    } else {
        bench::read(channel, &tree, options).await
    };
    if let Some(path) = &args.json {
        write_json(path, &report).map_err(|source| CliError::Json {
            path: path.clone(),
            source,
        })?;
    }
    Ok(Finished::Blobs(report))
}

/// Nanoseconds since 1970: another seed every run.
fn clock_seed() -> u64 {
    let since = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH);
    since.map_or(0, |d| d.as_nanos() as u64)
}

fn write_json(path: &Path, report: &Report) -> std::io::Result<()> {
    let mut text = serde_json::to_vec_pretty(report)?;
    text.push(b'\n');
    std::fs::write(path, text)
}
