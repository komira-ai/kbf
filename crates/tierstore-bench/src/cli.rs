//! The command line: seven verbs, each a thin call into its module.
//!
//! Exit status: 0 on success, 2 when the verb stopped (including a verb not built yet) and for
//! usage errors, as clap reports them.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};

use crate::cells::cold_put::ColdPutPlan;
use crate::cells::{self, CellContext, CellId, CellPlan};
use crate::cleanup;
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
    /// Delete the objects a run of cell cold-put wrote, as its request log lists them.
    Cleanup(CleanupArgs),
    /// Serve the fault proxy in front of a store.
    FaultProxy(ProxyArgs),
    /// Check, against the before-snapshot, that nothing the run made is left and nothing live
    /// changed.
    VerifyTeardown(TeardownArgs),
    /// Print one table from the results of a result directory.
    Report(ReportArgs),
    /// Print `tierstore-bench sha256 <hex>`, the SHA-256 of this executable (CI logs it so the
    /// binary a run uses can be checked against the build).
    SelfDigest,
}

/// The line `self-digest` prints.
///
/// # Errors
/// [`BenchError::Io`] if the executable cannot be found or read.
pub fn self_digest_line() -> Result<String, BenchError> {
    use sha2::{Digest as _, Sha256};
    let path = std::env::current_exe().map_err(|source| BenchError::Io {
        path: PathBuf::from("current executable"),
        source,
    })?;
    let bytes = std::fs::read(&path).map_err(|source| BenchError::Io { path, source })?;
    Ok(format!(
        "tierstore-bench sha256 {}",
        hex::encode(Sha256::digest(bytes))
    ))
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
    /// Cell cold-put: names the run; every key is under `<prefix>/<run-id>/`.
    #[arg(long, value_name = "ID")]
    pub run_id: Option<String>,
    /// Cell cold-put: the first part of every key's prefix.
    #[arg(long, default_value = "bench/c1")]
    pub prefix: String,
    /// Cell cold-put: puts per level.
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u64).range(1..))]
    pub puts: u64,
    /// Cell cold-put: seconds a level may take.
    #[arg(long, default_value_t = 600)]
    pub level_secs: u64,
    /// Cell cold-put: puts between two reads of the machine's load (0: only before a level).
    #[arg(long, default_value_t = 50)]
    pub gate_every: u64,
    /// Cell cold-put: instead of the levels, the concurrent-create check: `--in-flight`
    /// writers (one value) put different bytes to one key together.
    #[arg(long)]
    pub same_key: bool,
    /// Cell cold-put, with `--same-key`: release the writers at this time (milliseconds since
    /// the Unix epoch), so several machines start together.
    #[arg(long, value_name = "MS")]
    pub start_at_unix_ms: Option<u64>,
    /// Cell cold-put: the run ends before the next put when this file exists.
    #[arg(long, value_name = "FILE")]
    pub stop_file: Option<PathBuf>,
    /// The commit this binary was built from, copied into the results.
    #[arg(long)]
    pub git_sha: Option<String>,
    /// SHA-256 of this binary, copied into the results.
    #[arg(long)]
    pub binary_digest: Option<String>,
}

impl RunArgs {
    /// The plan the cells are given.
    #[must_use]
    pub fn plan(&self) -> CellPlan {
        CellPlan {
            passes: self.passes,
            in_flight: self.in_flight.clone(),
            object_bytes: self.object_bytes.clone(),
            cold_put: ColdPutPlan {
                run_id: self.run_id.clone().unwrap_or_default(),
                prefix: self.prefix.clone(),
                puts: self.puts,
                level_secs: self.level_secs,
                gate_every: self.gate_every,
                same_key: self.same_key,
                start_at_unix_ms: self.start_at_unix_ms,
                stop_file: self.stop_file.clone(),
            },
        }
    }

    /// Where the numbers come from, as far as the flags say.
    #[must_use]
    pub fn provenance(&self) -> Provenance {
        Provenance {
            run_id: self.run_id.clone().unwrap_or_default(),
            git_sha: self.git_sha.clone().unwrap_or_default(),
            binary_digest: self.binary_digest.clone().unwrap_or_default(),
            tool_versions: BTreeMap::new(),
            hardware: BTreeMap::new(),
        }
    }
}

/// Flags of `cleanup`.
#[derive(Debug, Args)]
pub struct CleanupArgs {
    #[command(flatten)]
    pub config: ConfigArgs,
    /// The run to clean up; the same `--prefix` and id the run used.
    #[arg(long, value_name = "ID")]
    pub run_id: String,
    /// The first part of the run's key prefix.
    #[arg(long, default_value = "bench/c1")]
    pub prefix: String,
    /// Deletes in flight.
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u32).range(1..))]
    pub in_flight: u32,
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
        Command::Preflight(args) => {
            preflight::run(&RunConfig::load(&args)?).await?;
            Ok(())
        }
        Command::Provision(args) => {
            let mode = if args.apply {
                Mode::Apply
            } else {
                Mode::DryRun
            };
            provision::run(&RunConfig::load(&args.config)?, mode).await?;
            Ok(())
        }
        Command::Run(args) => {
            let config = RunConfig::load(&args.config)?;
            let log = std::fs::create_dir_all(&config.result_dir)
                .map_err(|source| BenchError::Io {
                    path: config.result_dir.clone(),
                    source,
                })
                .and_then(|()| RequestLog::create(&config.result_dir.join("requests.jsonl")))?;
            let plan = args.plan();
            let ctx = CellContext {
                config,
                log: Arc::new(log),
                provenance: args.provenance(),
            };
            for id in args.cells {
                cells::run(id, &ctx, &plan).await?;
            }
            Ok(())
        }
        Command::Cleanup(args) => {
            let config = RunConfig::load(&args.config)?;
            let log = cleanup::open_log(&config)?;
            let report =
                cleanup::run(&config, &log, &args.prefix, &args.run_id, args.in_flight).await?;
            println!(
                "cleanup: written={} (acknowledged {}, unanswered {}) deleted={} failed={} remaining_under_prefix={}",
                report.written(),
                report.put_ok,
                report.put_unanswered,
                report.deleted,
                report.failed,
                report.remaining
            );
            if report.failed > 0 {
                return Err(BenchError::Halted(format!(
                    "{} deletes were not acknowledged; run cleanup again",
                    report.failed
                )));
            }
            Ok(())
        }
        Command::FaultProxy(args) => {
            proxy::run(ProxyConfig {
                listen: args.listen,
                upstream: args.upstream,
                mode: args.mode,
            })
            .await
        }
        Command::VerifyTeardown(args) => {
            let config = RunConfig::load(&args.config)?;
            teardown::run(&config, &args.before).await?;
            Ok(())
        }
        Command::Report(args) => report::render(&args.result_dir).map(|_| ()),
        Command::SelfDigest => {
            println!("{}", self_digest_line()?);
            Ok(())
        }
    }
}
