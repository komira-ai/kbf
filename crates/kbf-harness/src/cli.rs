//! The command line of `kbf-beta`, in the library so that tests run it without a process.
//!
//! `kbf-beta b0 --bin-dir <dir> --buck2 <path> --project <dir> --work <dir> [...]` runs
//! scenario B0 ([`crate::b0`]) and exits 0 when every gate passed, 1 when one did not,
//! and 2 for a command line it refuses or a run directory it cannot make.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};

use crate::b0::{self, B0, Ended, Mutant};
use crate::envoy::{ENV_VAR, REQUIRE_VAR, required};
use crate::interrupt::Interrupt;
use crate::procs::HANG;

/// The longest one `buck2 kill` is waited for.
const KILL_BOUND: Duration = Duration::from_secs(10);

#[derive(Parser)]
#[command(
    name = "kbf-beta",
    about = "The beta scenarios over a farm of real processes."
)]
struct Cli {
    #[command(subcommand)]
    scenario: Scenario,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum MutantFlag {
    /// Node 1's cold store is told its disk is nearly full.
    ShrinkNodeCap,
    /// Every node's cold store is told its disk is nearly full.
    ShrinkEveryCap,
    /// Node 2 is killed halfway through the load.
    KillNodeMidway,
}

#[derive(Subcommand)]
enum Scenario {
    /// B0, `farm_under_build_load`: the farm under Buck2 builds, no update and no kill.
    B0 {
        /// The directory holding `kbf` and `kbf-m2-act`.
        #[arg(long)]
        bin_dir: PathBuf,
        /// The pinned buck2.
        #[arg(long)]
        buck2: PathBuf,
        /// An Envoy binary to put in front of the frontend (default: `KBF_TEST_ENVOY`).
        #[arg(long)]
        envoy: Option<PathBuf>,
        /// The sample project (`crates/kbf-it/beta/buck2`).
        #[arg(long)]
        project: PathBuf,
        /// The run's directory (fresh): the farm, `logs/` and `beta-result.json`.
        #[arg(long)]
        work: PathBuf,
        /// How long the loops start builds for, in seconds.
        #[arg(long, default_value_t = 300)]
        duration_secs: u64,
        /// How long to wait after the load before the last scrape, in seconds.
        #[arg(long, default_value_t = 20)]
        settle_secs: u64,
        /// Build loops running at once (each has a Buck2 daemon of its own).
        #[arg(long, default_value_t = 4)]
        concurrency: usize,
        /// The longest one build may take, in seconds.
        #[arg(long, default_value_t = 300)]
        build_timeout_secs: u64,
        /// How long the pause action sleeps, in seconds.
        #[arg(long, default_value_t = 30)]
        pause_secs: u64,
        /// The size of each upload action's output, in KiB.
        #[arg(long, default_value_t = 256)]
        fill_kib: u64,
        /// Appends the gate table here (`$GITHUB_STEP_SUMMARY`).
        #[arg(long)]
        summary: Option<PathBuf>,
        /// Makes the farm worse in one way the gates must catch.
        #[arg(long, value_enum)]
        mutant: Option<MutantFlag>,
    },
}

impl From<MutantFlag> for Mutant {
    fn from(flag: MutantFlag) -> Self {
        match flag {
            MutantFlag::ShrinkNodeCap => Mutant::ShrinkNodeCap,
            MutantFlag::ShrinkEveryCap => Mutant::ShrinkEveryCap,
            MutantFlag::KillNodeMidway => Mutant::KillNodeMidway,
        }
    }
}

/// The longest a run may take: the load and the build under way at its end, and a minute to
/// start and stop it. Saturating: a command line with absurd seconds is a bound far away, not
/// an overflow.
fn wall_bound(duration_secs: u64, build_timeout_secs: u64) -> Duration {
    Duration::from_secs(
        duration_secs
            .saturating_add(build_timeout_secs)
            .saturating_add(60),
    )
}

/// Runs `kbf-beta` on `args` (the first is the program's name), with `env` the lookup of
/// an environment variable.
pub fn run(
    args: impl IntoIterator<Item = OsString>,
    env: impl Fn(&str) -> Option<String>,
) -> ExitCode {
    ExitCode::from(status(args, env))
}

/// [`run`]'s exit status: 0 for a run whose gates all passed (and for `--help`), 1 for one
/// that failed a gate, 2 for a command line refused or a run directory that cannot be made.
pub fn status(
    args: impl IntoIterator<Item = OsString>,
    env: impl Fn(&str) -> Option<String>,
) -> u8 {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            // `--help` is not a failure.
            let _ = e.print();
            return u8::from(e.use_stderr()) * 2;
        }
    };
    let Scenario::B0 {
        bin_dir,
        buck2,
        envoy,
        project,
        work,
        duration_secs,
        settle_secs,
        concurrency,
        build_timeout_secs,
        pause_secs,
        fill_kib,
        summary,
        mutant,
    } = cli.scenario;
    let args = B0 {
        kbf: bin_dir.join("kbf"),
        act: bin_dir.join("kbf-m2-act"),
        buck2,
        envoy,
        envoy_var: ENV_VAR.to_owned(),
        project,
        work,
        duration: Duration::from_secs(duration_secs),
        settle: Duration::from_secs(settle_secs),
        concurrency,
        build_timeout: Duration::from_secs(build_timeout_secs),
        kill_timeout: KILL_BOUND,
        wall_bound: wall_bound(duration_secs, build_timeout_secs),
        pause_secs,
        fill_kib,
        scrape_every: Duration::from_secs(1),
        summary,
        mutant: mutant.map(Mutant::from),
        extra_flags: Vec::new(),
        farm_hang: HANG,
        require_envoy: required(env(REQUIRE_VAR).as_deref()),
        interrupt: Interrupt::process(),
    };
    match b0::run(&args) {
        Ok(ended) => u8::from(ended == Ended::Failed),
        Err(e) => {
            eprintln!("kbf-beta: {e}");
            2
        }
    }
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;
