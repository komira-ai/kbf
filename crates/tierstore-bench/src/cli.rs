//! The command line: seven verbs, each a thin call into its module.
//!
//! Exit status: 0 on success, 2 when the verb stopped and for usage errors, as clap reports them.

use std::net::SocketAddr;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::cells::budget::{Campaign, lock_path};
use crate::cells::engine::FaultSwitch;
use crate::cells::{self, CellContext, CellId, CellPlan};
use crate::coldstate::{ColdStateSource, NoColdState, OpsColdState};
use crate::config::{ConfigArgs, RunConfig};
use crate::error::BenchError;
use crate::faultfile::FileSwitch;
use crate::preflight::host::SystemHost;
use crate::preflight::ops::OpsFile;
use crate::preflight::{self, PreflightReport};
use crate::provision::{self, Mode, Supply};
use crate::proxy::{self, FaultMode, ProxyConfig};
use crate::requestlog::RequestLog;
use crate::result::Provenance;
use crate::role::Role;
use crate::{livedir, report, teardown};

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
    /// Print the test prefix, test bucket names and scoped account ids the run file resolves to,
    /// one `key=value` per line (`test_prefix`, `bucket`, `account`). Sends nothing to any store.
    Plan(ConfigArgs),
    /// Create the test buckets, scoped accounts and quotas (a dry run unless `--apply`).
    Provision(ProvisionArgs),
    /// Run cells.
    Run(Box<RunArgs>),
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
    /// The buckets, quotas, policies and accounts were made by a held script: make none of them,
    /// check that each bucket is there with a quota of at most 30 GiB, and prove the account (it
    /// reaches its own bucket, it is refused a live one).
    #[arg(long)]
    pub premade: bool,
}

/// A flag that is on or off, spelled out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Switch {
    /// Do it.
    On,
    /// Do not.
    Off,
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
    /// Whether the put cells (`cold-put`, `a1`) add their open-loop arms: 19 KiB at 20 a second
    /// for 600 s and 1 MiB at 10 a second for 300 s, about 60% of the bytes of a 1 MiB run.
    /// `off` leaves them out of the plan, the ledger is asked to admit the plan without them,
    /// and the report and the provenance of every result say so.
    #[arg(long, value_enum, default_value_t = Switch::On, value_name = "on|off")]
    pub open_loop: Switch,
    /// At most this many objects per pass for the multipart arms of `cold-put` (`direct-mp`,
    /// `instance`): a cap on the plan's count (1,000 up to 1 MiB), never a raise. Each multipart object is three requests to the
    /// store, so a slow cold store may not fit the plan's count in a window. The other arms
    /// (`direct`, the open-loop arms, the other cells) keep theirs. An arm with fewer than 1,000
    /// objects per pass reports no p99, and its result says so.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u64).range(1..))]
    pub multipart_objects: Option<u64>,
    /// Names the run in every result and in the keys it writes (role names and digits; no host).
    #[arg(long)]
    pub run_id: String,
    /// Which try of the run this is, from 1. The results of try `N` are
    /// `<cell>-<arm>-a<N>.json`; a run never writes over a result file, so a retry of a cell and
    /// arm that has a result is run with the next number, and every attempt is kept. The report
    /// reads them all and labels them.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
    pub attempt: u32,
    /// The commit this binary was built from, copied into every result.
    #[arg(long)]
    pub git_sha: String,
    /// The control file of the running fault proxy (its `--control`): the fault arms of the
    /// cells that have them switch it. Without it only the healthy arms run.
    #[arg(long, value_name = "FILE", requires = "fault_result_dir")]
    pub fault_control: Option<PathBuf>,
    /// The result directory of the running fault proxy (its `--result-dir`), where its counters
    /// are.
    #[arg(long, value_name = "DIR", requires = "fault_control")]
    pub fault_result_dir: Option<PathBuf>,
    /// Seconds a fault set through `--fault-control` lasts before the proxy falls back to `pass`
    /// on its own, so a run that dies with a fault in force does not leave the store refused. It
    /// must be longer than the longest stretch a cell holds one fault: a fault that ends early
    /// turns the rest of that arm into a healthy one.
    #[arg(long, value_name = "SECS", default_value = "300")]
    pub fault_ttl_secs: NonZeroU64,
    /// The campaign ledger: the file that counts the bytes written to the cold store across all
    /// runs of a campaign, kept outside every result directory. It must exist, unless
    /// `--new-campaign` says this run starts the campaign.
    #[arg(long, value_name = "FILE")]
    pub campaign_ledger: PathBuf,
    /// Create the campaign ledger (refused if the file is already there, so a ledger is never
    /// reset by a flag left in a command line).
    #[arg(long)]
    pub new_campaign: bool,
}

impl RunArgs {
    /// Whether the open-loop arms are in the plan (`--open-loop`).
    #[must_use]
    pub fn open_loop(&self) -> bool {
        self.open_loop == Switch::On
    }

    /// What the flags ask of a cell.
    #[must_use]
    pub fn plan(&self) -> CellPlan {
        CellPlan {
            passes: self.passes,
            in_flight: self.in_flight.clone(),
            object_bytes: self.object_bytes.clone(),
            open_loop: self.open_loop(),
            multipart_objects: self.multipart_objects,
        }
    }

    /// The switch of the fault proxy `--fault-control` names, with `--fault-ttl-secs`.
    #[must_use]
    pub fn fault_switch(&self) -> Option<FileSwitch> {
        let ttl = Duration::from_secs(self.fault_ttl_secs.get());
        self.fault_control
            .clone()
            .zip(self.fault_result_dir.as_deref())
            .map(|(control, dir)| FileSwitch::new(control, dir).with_ttl(ttl))
    }
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

/// The provenance of a run: the facts of the passed preflight in the result directory, with the
/// run's id and commit.
fn provenance(
    result_dir: &Path,
    run_id: &str,
    git_sha: &str,
    attempt: u32,
    open_loop: bool,
) -> Result<Provenance, BenchError> {
    let path = result_dir.join(preflight::REPORT_FILE);
    let text = std::fs::read(&path).map_err(|source| BenchError::Io { path, source })?;
    let report: PreflightReport = serde_json::from_slice(&text)
        .map_err(|_| BenchError::Result("preflight.json is not a preflight report".to_owned()))?;
    preflight::all_passed("run", &report.checks)?;
    Ok(Provenance {
        attempt,
        open_loop,
        ..report.provenance(run_id, git_sha)
    })
}

/// Where the run reads the cold store's state from: the preflight's source on this machine, or
/// nothing if the run has no cold store.
fn cold_source(config: &RunConfig, ops: &OpsFile) -> Arc<dyn ColdStateSource> {
    match config.stores.get(&Role::Cold) {
        Some(spec) => Arc::new(
            OpsColdState::new(
                Arc::new(SystemHost),
                ops.clone(),
                spec.clone(),
                config.result_dir.clone(),
            )
            .with_max_age(config.cold_state_max_age),
        ),
        None => Arc::new(NoColdState),
    }
}

/// Runs the verb.
///
/// # Errors
/// [`BenchError`]: what stopped the verb.
pub async fn run(cli: Cli) -> Result<(), BenchError> {
    match cli.command {
        Command::Preflight(args) => preflight::run(&RunConfig::load(&args)?).await.map(|_| ()),
        Command::Plan(args) => {
            for line in RunConfig::load(&args)?.plan_lines() {
                println!("{line}");
            }
            Ok(())
        }
        Command::Provision(args) => {
            let mode = if args.apply {
                Mode::Apply
            } else {
                Mode::DryRun
            };
            let supply = if args.premade {
                Supply::Premade
            } else {
                Supply::Create
            };
            provision::run(&RunConfig::load(&args.config)?, mode, supply)
                .await
                .map(|_| ())
        }
        Command::Run(args) => {
            let args = *args;
            let config = RunConfig::load(&args.config)?;
            // Before anything is created: the result directory, the ledger and its lock stay
            // clear of the live tierstore's directories.
            let ops = OpsFile::load(&SystemHost, &config.result_dir)?;
            livedir::check_outside(
                &[
                    ("result directory", config.result_dir.as_path()),
                    ("campaign ledger", args.campaign_ledger.as_path()),
                    (
                        "campaign ledger lock",
                        lock_path(&args.campaign_ledger).as_path(),
                    ),
                ],
                &ops.live.dirs,
            )?;
            // The result directory is there: the ops file was read from it.
            let log = RequestLog::create(&config.result_dir.join("requests.jsonl"))?;
            let provenance = provenance(
                &config.result_dir,
                &args.run_id,
                &args.git_sha,
                args.attempt,
                args.open_loop(),
            )?;
            // Before a request is sent: the ledger is begun or found, or the run stops.
            // The lock is held until the arm ends, the run with it.
            let (campaign, _lock) = Campaign::begin(
                &args.campaign_ledger,
                args.new_campaign,
                &config.result_dir,
                config.bytes,
            )?;
            let faults = args
                .fault_switch()
                .map(|switch| Arc::new(switch) as Arc<dyn FaultSwitch>);
            let plan = args.plan();
            let ctx = CellContext {
                provenance,
                cold_state: cold_source(&config, &ops),
                config,
                log: Arc::new(log),
                campaign,
            };
            for id in args.cells {
                cells::run_with_faults(id, &ctx, &plan, faults.clone())
                    .await
                    .map(|_| ())?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cells::testkit;
    use crate::result::Window;

    fn run_args(extra: &[&str]) -> Result<RunArgs, clap::Error> {
        let mut argv = vec![
            "tierstore-bench",
            "run",
            "--result-dir",
            "r",
            "--cell",
            "cold-put",
            "--run-id",
            "run-1",
            "--git-sha",
            "abc",
            "--campaign-ledger",
            "ledger.json",
        ];
        argv.extend_from_slice(extra);
        match Cli::try_parse_from(argv)?.command {
            Command::Run(args) => Ok(*args),
            other => panic!("not the run verb: {other:?}"),
        }
    }

    /// Catches `--open-loop` that is off by default (a changed behaviour for every run in use),
    /// that cannot be turned off, or that takes a word other than `on` and `off` for an answer.
    #[test]
    fn open_loop_is_on_unless_set_off_and_takes_on_or_off_only() {
        assert!(run_args(&[]).unwrap().open_loop());
        assert!(run_args(&["--open-loop", "on"]).unwrap().open_loop());
        assert!(!run_args(&["--open-loop", "off"]).unwrap().open_loop());
        assert!(!run_args(&["--open-loop=off"]).unwrap().open_loop());
        for bad in ["true", "false", "yes", "0", "", "ON"] {
            assert!(run_args(&["--open-loop", bad]).is_err(), "{bad:?}");
        }
        assert!(run_args(&["--open-loop"]).is_err());
    }

    /// Catches `--multipart-objects` that changes the plan of a run without it, that does not
    /// reach the plan of a run with it, or that takes zero or a word.
    #[test]
    fn multipart_objects_is_absent_by_default_and_reaches_the_plan() {
        assert_eq!(run_args(&[]).unwrap().plan().multipart_objects, None);
        let args = run_args(&["--multipart-objects", "50"]).unwrap();
        assert_eq!(args.plan().multipart_objects, Some(50));
        for bad in ["0", "-1", "many", ""] {
            assert!(run_args(&["--multipart-objects", bad]).is_err(), "{bad:?}");
        }
    }

    /// Catches the provenance of a run not carrying the setting the run was given: it is what the
    /// report and every result file say about the arms.
    #[test]
    fn the_provenance_of_a_run_records_the_open_loop_setting() {
        let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "tb-cli-provenance");
        std::fs::create_dir_all(dir.path()).unwrap();
        let report = serde_json::to_vec(&PreflightReport::default()).unwrap();
        std::fs::write(dir.path().join(preflight::REPORT_FILE), report).unwrap();
        let of = |open_loop| provenance(dir.path(), "run-1", "abc", 3, open_loop).unwrap();
        let (on, off) = (of(true), of(false));
        assert_eq!((on.open_loop, off.open_loop), (true, false));
        assert_eq!(
            (off.run_id.as_str(), off.git_sha.as_str(), off.attempt),
            ("run-1", "abc", 3)
        );
    }

    /// Catches the source of the cold state being chosen by the wrong test: a run with no cold
    /// role reads nothing, and a run with one reads through the preflight's source, which here,
    /// with no admin key to read, says it cannot tell (so the run would stop).
    #[test]
    fn a_cold_role_is_read_through_the_ops_file_and_no_cold_role_is_not_read() {
        let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "tb-cli-cold-source");
        let ops: OpsFile = serde_json::from_value(serde_json::json!({
            "tierstore": {"binary": "/x/t", "sha256": "00", "version": "0.0.37", "plan_flags": []},
            "live": {"unit": "l.service", "binary": "/x/l", "port": 9300, "dirs": ["/x/live"]},
            "test_instance": {"unit": "t.service", "port": 9411, "dirs": ["/x/state"]},
            "admin": {"cold": "/x/absent-key.json"},
            "mc": "/x/absent-mc"
        }))
        .unwrap();
        let none = testkit::ctx(dir.path(), &[], Window::Busy).config;
        assert_eq!(cold_source(&none, &ops).read().unwrap(), None);
        let cold = testkit::ctx(
            dir.path(),
            &[(Role::Cold, "http://127.0.0.1:9")],
            Window::Busy,
        );
        let err = cold_source(&cold.config, &ops).read().unwrap_err();
        assert_eq!(
            err.to_string(),
            "cold state: a file cannot be read: NotFound"
        );
    }

    /// Catches `--cold-state-max-age-secs` not reaching the source: a status file 20 s old is
    /// unreadable under a limit of 10 s and read under a limit of 60 s, through `cold_source`.
    #[test]
    fn the_cold_state_max_age_reaches_the_source() {
        let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "tb-cli-cold-age");
        std::fs::create_dir_all(dir.path()).unwrap();
        let file = dir.path().join("heal.json");
        let stamp = crate::s3::unix_ms().saturating_sub(20_000);
        std::fs::write(
            &file,
            format!(r#"{{"read_unix_ms": {stamp}, "healing": false, "drives": []}}"#),
        )
        .unwrap();
        let ops: OpsFile = serde_json::from_value(serde_json::json!({
            "tierstore": {"binary": "/x/t", "sha256": "00", "version": "0.0.37", "plan_flags": []},
            "live": {"unit": "l.service", "binary": "/x/l", "port": 9300, "dirs": ["/x/live"]},
            "test_instance": {"unit": "t.service", "port": 9411, "dirs": ["/x/state"]},
            "cold_heal_status_file": file,
            "mc": "/x/absent-mc"
        }))
        .unwrap();
        let mut cold = testkit::ctx(
            dir.path(),
            &[(Role::Cold, "http://127.0.0.1:9")],
            Window::Busy,
        )
        .config;
        cold.cold_state_max_age = Duration::from_secs(10);
        let err = cold_source(&cold, &ops).read().unwrap_err().to_string();
        assert!(err.contains("over the limit of 10 s"), "{err}");
        cold.cold_state_max_age = Duration::from_secs(60);
        assert!(cold_source(&cold, &ops).read().unwrap().is_some());
    }
}
