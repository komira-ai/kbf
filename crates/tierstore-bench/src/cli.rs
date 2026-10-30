//! The command line: six verbs, each a thin call into its module.
//!
//! Exit status: 0 on success, 2 when the verb stopped (including a verb not built yet) and for
//! usage errors, as clap reports them.

use std::net::SocketAddr;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

use crate::cells::{self, CellContext, CellId, CellPlan};
use crate::config::{ConfigArgs, RunConfig};
use crate::error::BenchError;
use crate::provision::{self, Mode};
use crate::proxy::{self, FaultMode, ProxyConfig};
use crate::requestlog::RequestLog;
use crate::result::Provenance;
use crate::{preflight, report, teardown};

/// A measurement tool for tierstore.
#[derive(Debug, Parser)]
#[command(name = "tierstore-bench", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// The verbs.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Read-only checks before anything is written; writes the before-snapshot.
    Preflight(ConfigArgs),
    /// Create the test buckets, scoped accounts and quotas (a dry run unless `--apply`).
    Provision(ProvisionArgs),
    /// Run cells.
    Run(RunArgs),
    /// Serve the fault proxy in front of a store.
    FaultProxy(ProxyArgs),
    /// Check, against the before-snapshot, that nothing the run made is left and nothing live
    /// changed.
    VerifyTeardown(TeardownArgs),
    /// Print one table from the results of a result directory.
    Report(ReportArgs),
}

/// Flags of `provision`.
#[derive(Debug, Args)]
pub struct ProvisionArgs {
    #[command(flatten)]
    pub config: ConfigArgs,
    /// Create what the plan lists; without it the verb only prints it. The admin key pair is
    /// read from standard input either way.
    #[arg(long)]
    pub apply: bool,
}

/// Flags of `run`.
#[derive(Debug, Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub config: ConfigArgs,
    /// A cell to run; repeatable, run in the order given.
    #[arg(long = "cell", required = true, value_enum)]
    pub cells: Vec<CellId>,
    /// Passes per arm, the discarded warm-up included.
    #[arg(long, default_value_t = 6, value_parser = clap::value_parser!(u32).range(2..))]
    pub passes: u32,
    /// Requests in flight, one arm each (comma separated).
    #[arg(long, value_delimiter = ',', default_value = "1")]
    pub in_flight: Vec<u32>,
    /// Object sizes in bytes, one arm each (comma separated).
    #[arg(long, value_delimiter = ',')]
    pub object_bytes: Vec<u64>,
}

/// Flags of `fault-proxy`.
#[derive(Debug, Args)]
pub struct ProxyArgs {
    /// Loopback address to listen on.
    #[arg(long)]
    pub listen: SocketAddr,
    /// The store's endpoint, `http://host:port`.
    #[arg(long)]
    pub upstream: String,
    /// `pass`, `refuse`, `blackhole`, `latency=MS`, `bandwidth=BYTES` or `skew=SECS`.
    #[arg(long, default_value = "pass")]
    pub mode: FaultMode,
    /// Seconds a fault lasts before the proxy falls back to `pass` (default: until changed).
    #[arg(long, value_name = "SECS")]
    pub ttl_secs: Option<NonZeroU64>,
    /// A file of one line, `MODE [ttl=SECS]`, read every tick and applied when it changes.
    #[arg(long, value_name = "FILE")]
    pub control: Option<PathBuf>,
    /// Where `proxy-counters.json` is written.
    #[arg(long, value_name = "DIR")]
    pub result_dir: Option<PathBuf>,
    /// Allow `skew=SECS`, which shifts `Date` and `Last-Modified` of the answers.
    #[arg(long)]
    pub allow_date_rewrite: bool,
}

/// Flags of `verify-teardown`.
#[derive(Debug, Args)]
pub struct TeardownArgs {
    #[command(flatten)]
    pub config: ConfigArgs,
    /// The before-snapshot preflight wrote.
    #[arg(long, value_name = "FILE")]
    pub before: PathBuf,
}

/// Flags of `report`.
#[derive(Debug, Args)]
pub struct ReportArgs {
    /// The result directory to read.
    #[arg(long, value_name = "DIR")]
    pub result_dir: PathBuf,
}

/// Runs the verb.
///
/// # Errors
/// [`BenchError`]: what stopped the verb.
pub async fn run(cli: Cli) -> Result<(), BenchError> {
    match cli.command {
        Command::Preflight(args) => preflight::run(&RunConfig::load(&args)?).await.map(|_| ()),
        Command::Provision(args) => {
            let mode = if args.apply {
                Mode::Apply
            } else {
                Mode::DryRun
            };
            provision::run(&RunConfig::load(&args.config)?, mode)
                .await
                .map(|_| ())
        }
        Command::Run(args) => {
            let config = RunConfig::load(&args.config)?;
            let log = std::fs::create_dir_all(&config.result_dir)
                .map_err(|source| BenchError::Io {
                    path: config.result_dir.clone(),
                    source,
                })
                .and_then(|()| RequestLog::create(&config.result_dir.join("requests.jsonl")))?;
            let plan = CellPlan {
                passes: args.passes,
                in_flight: args.in_flight,
                object_bytes: args.object_bytes,
            };
            let ctx = CellContext {
                config,
                log: Arc::new(log),
                provenance: Provenance::unrecorded(),
            };
            for id in args.cells {
                cells::run(id, &ctx, &plan).await.map(|_| ())?;
            }
            Ok(())
        }
        Command::FaultProxy(args) => {
            proxy::run(ProxyConfig {
                mode: args.mode,
                ttl: args.ttl_secs.map(|n| Duration::from_secs(n.get())),
                control: args.control,
                result_dir: args.result_dir,
                allow_date_rewrite: args.allow_date_rewrite,
                ..ProxyConfig::new(args.listen, args.upstream)
            })
            .await
        }
        Command::VerifyTeardown(args) => {
            let config = RunConfig::load(&args.config)?;
            teardown::run(&config, &args.before).await.map(|_| ())
        }
        Command::Report(args) => {
            let table = report::render(&args.result_dir)?;
            print!("{table}");
            Ok(())
        }
    }
}
