//! The `kbf-bench` command line, and the run it describes.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use clap::{Parser, Subcommand, ValueEnum};

use crate::bench::{self, Method, Options};
use crate::connect::{self, ConnectError, Target};
use crate::report::Report;
use crate::tree::{self, Shape, TreeError};

/// Times blob uploads to (and reads from) a kbf-server's CAS.
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
        /// ones. The daemon does not.
        #[arg(long, value_enum, default_value_t = OnOff::Off)]
        find_missing: OnOff,
    },
    /// Reads the blobs of the tree an upload with the same tree flags and `--seed`
    /// sent, each checked against its digest.
    Read {
        #[command(flatten)]
        run: RunArgs,
    },
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
    /// The most requests in flight at once (the daemon uploads 64 at a time).
    #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(u64).range(1..))]
    pub in_flight: u64,
    /// The most blob data one batch call carries (the front allows 4 MiB); a larger
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

/// Builds the tree, makes the run `cli` describes and writes its JSON; returns the
/// report, complete or not.
///
/// # Errors
/// `read` without `--seed`; the tree cannot be made; the connection cannot be set up;
/// the JSON cannot be written.
pub async fn run(cli: &Cli) -> Result<Report, CliError> {
    let (args, find_missing, upload) = match &cli.command {
        Command::Upload { run, find_missing } => (run, *find_missing == OnOff::On, true),
        Command::Read { run } => (run, false, false),
    };
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
    Ok(report)
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
