//! The command line of `kbf-installer`: the verbs, their flags and what each one needs.
//!
//! **Every verb is named here and nowhere else.** [`Verb`] is the one enum and [`Cli`] the one
//! clap definition; the dispatcher ([`crate::app`]) matches on [`Verb`] and the tables below
//! (`name`, `needs_root`, `phase`) are `match`es on it. Every argument other than the verb is
//! a named flag: there is no positional argument. When the CLI moves to `--action=<verb>`,
//! this file is the one that changes.

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::CommitId;
use crate::record::ActivationId;

#[derive(Parser)]
#[command(
    name = "kbf-installer",
    version,
    about = "The one root tool for setting up a kbf host and for updating it",
    long_about = "The one root tool for setting up a kbf host and for updating it.\n\n\
Mutating verbs only say what they would do unless --apply is given. Exit codes: 0 ok, 1 failed, \
2 usage or a missing prerequisite, 3 refused (verification or policy), 4 busy (another run \
holds the lock), 5 needs an operator."
)]
pub struct Cli {
    /// Print the result as one JSON object on standard output.
    #[arg(long, global = true)]
    pub json: bool,
    /// Do what the verb says. Without it a mutating verb is a dry run.
    #[arg(long, global = true)]
    pub apply: bool,
    /// The directory the host's paths are under (a sandbox for tests; absent from release builds).
    #[cfg(feature = "test-sandbox")]
    #[arg(long, global = true, default_value = "/", hide = true)]
    pub root: std::path::PathBuf,
    #[command(subcommand)]
    pub verb: Verb,
}

#[derive(Subcommand)]
pub enum Verb {
    /// First install: accounts, directories, installer.toml, the first release.
    Setup(SetupArgs),
    /// What is installed, staged and pending, and the last refusal.
    Status,
    /// Run by kbf-installer.service: act on the orders kbf wrote, revert an expired swap.
    Run(RunArgs),
    /// Order an update of this host's own frontend (root, inline).
    Update(UpdateArgs),
    /// Keep the pending swap, naming its activation.
    Confirm(ActivationArgs),
    /// Undo the pending swap, naming its activation.
    Revert(ActivationArgs),
    /// Point `current` at `previous`, with no confirm window.
    Rollback,
    /// Remove releases beyond current, previous and one more.
    Gc,
    /// Run before each start of a release on trial; counts the start.
    BootCheck(BootArgs),
    /// For maintainer scripts: take the lock; exit 5 while a swap is pending.
    Quiesce,
    /// Validate the installer's configuration, trust files and path chain.
    Check,
    #[command(hide = true)]
    Stage,
    #[command(hide = true)]
    Activate,
    #[command(hide = true)]
    FetchChild,
}

/// The `kbf` modes a host can be set up for. There is no `node` mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Mode {
    Frontend,
    Storage,
    Daemon,
    StorageAndDaemon,
    AllInOne,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ReleaseSource {
    Url,
    Frontend,
}

#[derive(Args)]
pub struct SetupArgs {
    /// The kbf mode this host runs (`kbf --mode=...`).
    #[arg(long, value_enum)]
    pub mode: Mode,
    /// Where releases are fetched from.
    #[arg(long, value_enum, default_value = "frontend")]
    pub release_source: ReleaseSource,
    /// The https base URL of the file server, for --release-source url.
    #[arg(long)]
    pub release_url: Option<String>,
    /// Refuse orders below this generation.
    #[arg(long)]
    pub min_generation: Option<u64>,
    /// The first release to install (40 hex characters).
    #[arg(long)]
    pub commit: Option<CommitId>,
    /// Enable the units without starting them.
    #[arg(long)]
    pub no_start: bool,
}

#[derive(Args)]
pub struct UpdateArgs {
    /// The release to update to (40 hex characters).
    #[arg(long)]
    pub commit: CommitId,
    /// Update this host's frontend. Node updates come on the frontend's order, not from here.
    #[arg(long, required = true)]
    pub frontend: bool,
}

#[derive(Args)]
pub struct ActivationArgs {
    /// The activation: `<epoch>-<order id>`.
    #[arg(long)]
    pub id: ActivationId,
}

#[derive(Args)]
pub struct RunArgs {
    /// Root's directory: status.json goes here; `inbox/` (the service user's) is beneath it.
    #[arg(long, default_value = "/var/lib/kbf-updater", value_parser = kbf_host::HostPath::new)]
    pub update_dir: kbf_host::HostPath,
    /// The systemd unit to restart after a swap (Linux).
    #[arg(
        long,
        conflicts_with = "launchd_label",
        required_unless_present = "launchd_label"
    )]
    pub unit: Option<String>,
    /// The launchd label to kickstart after a swap (macOS).
    #[arg(long)]
    pub launchd_label: Option<String>,
    /// Seconds between passes.
    #[arg(long, default_value_t = 3)]
    pub interval_secs: u64,
    /// The confirm window the boot check gives a release on trial (120 to 1800).
    #[arg(long, default_value_t = 900)]
    pub window_secs: u64,
    /// The starts a release on trial gets before it is reverted (Linux).
    #[arg(long, default_value_t = 3)]
    pub starts: u32,
    /// Stop after this many passes (a rehearsal or a test); without it, run until stopped.
    #[arg(long)]
    pub passes: Option<u64>,
}

#[derive(Args)]
pub struct BootArgs {
    #[arg(long, default_value_t = 900)]
    pub window_secs: u64,
}

impl Verb {
    /// The verb as typed.
    pub fn name(&self) -> &'static str {
        match self {
            Verb::Setup(_) => "setup",
            Verb::Status => "status",
            Verb::Run(_) => "run",
            Verb::Update(_) => "update",
            Verb::Confirm(_) => "confirm",
            Verb::Revert(_) => "revert",
            Verb::Rollback => "rollback",
            Verb::Gc => "gc",
            Verb::BootCheck(_) => "boot-check",
            Verb::Quiesce => "quiesce",
            Verb::Check => "check",
            Verb::Stage => "stage",
            Verb::Activate => "activate",
            Verb::FetchChild => "fetch-child",
        }
    }

    /// Every verb but `status` and `check` needs root.
    pub fn needs_root(&self) -> bool {
        !matches!(self, Verb::Status | Verb::Check)
    }

    /// The pull request of the contract (section 9) that builds the verb, `None` for the
    /// verbs that exist (`run`, `boot-check`).
    pub fn built_by(&self) -> Option<&'static str> {
        match self {
            Verb::Run(_) | Verb::BootCheck(_) => None,
            Verb::Setup(_) | Verb::Check => Some("P10"),
            Verb::Status => Some("P4"),
            Verb::Update(_) => Some("P13"),
            Verb::Confirm(_) | Verb::Revert(_) | Verb::Rollback | Verb::Gc | Verb::Quiesce => {
                Some("P5")
            }
            Verb::Activate => Some("P5"),
            Verb::Stage => Some("P8"),
            Verb::FetchChild => Some("P7"),
        }
    }
}
