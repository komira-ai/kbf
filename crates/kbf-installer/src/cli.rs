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
    version = crate::version::VERSION,
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
    Rollback(ServiceArgs),
    /// Remove the releases nothing needs: current, previous, staged, the swap on trial and any a
    /// process executes from stay, and so do the newest `--keep` others.
    Gc(GcArgs),
    /// Run before each start of a release on trial; counts the start.
    BootCheck(BootArgs),
    /// Adopt the old layout (`/usr/local/kbf`, the deploy kit's state) into the installer's
    /// tree, or put it back with `--rollback`. A dry run without `--apply`.
    Migrate(MigrateArgs),
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

impl Mode {
    /// The mode as `--mode` spells it (clap's own name for it, so there is one spelling).
    pub fn name(self) -> String {
        self.to_possible_value()
            .map_or_else(String::new, |v| v.get_name().to_owned())
    }

    /// Whether this mode keeps its state in the Postgres database the installer owns (the
    /// frontend and all-in-one modes; the others keep none).
    pub fn keeps_state_in_database(self) -> bool {
        matches!(self, Mode::Frontend | Mode::AllInOne)
    }
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
    /// installer.toml, under the host root: for `--mode frontend` and `all-in-one`, its
    /// `database` key names the state database whose schema `setup` creates (or brings
    /// forward); without `--apply` it only reports what it would do. Read for those two modes
    /// only. The same flag, with the same meaning, as on `run`.
    #[arg(long, default_value = "/etc/kbf/installer.toml", value_parser = kbf_host::HostPath::new)]
    pub installer_config: kbf_host::HostPath,
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
    #[command(flatten)]
    pub service: ServiceArgs,
}

/// The service a swap restarts, by its own flag (never from an order).
#[derive(Args)]
pub struct ServiceArgs {
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
}

impl ServiceArgs {
    /// Whether the service this installer restarts is the unit of a mode that keeps its state
    /// in the database (`kbf-frontend@<instance>.service`, `kbf-all-in-one.service` or its
    /// template `kbf-all-in-one@<instance>.service`): the installer is not told the host's
    /// mode, the unit it restarts is the one place that names it. A launchd label names no mode.
    pub fn runs_database_mode(&self) -> bool {
        self.unit.as_deref().is_some_and(|u| {
            u == "kbf-all-in-one.service"
                || (["kbf-frontend@", "kbf-all-in-one@"]
                    .iter()
                    .any(|p| u.starts_with(p))
                    && u.ends_with(".service"))
        })
    }

    /// How to restart the service. clap makes exactly one of the two flags present
    /// (`required_unless_present`, `conflicts_with`), so the label is there whenever the
    /// unit is not.
    pub fn restart(&self) -> crate::helper::restart::Restart {
        use crate::helper::restart::Restart;
        match &self.unit {
            Some(unit) => Restart::Systemd(unit.clone()),
            None => Restart::Launchd(self.launchd_label.clone().unwrap_or_default()),
        }
    }
}

#[derive(Args)]
pub struct GcArgs {
    /// The releases to keep besides the ones that must stay.
    #[arg(long, default_value_t = crate::helper::cycle::GC_KEEP)]
    pub keep: usize,
}

#[derive(Args)]
pub struct RunArgs {
    /// Root's directory: status.json goes here. Root-owned, mode 0755, so the node can read it.
    #[arg(long, default_value = "/var/lib/kbf-installer", value_parser = kbf_host::HostPath::new)]
    pub update_dir: kbf_host::HostPath,
    /// The order spool: owned by the service user, mode 0700, inside a root-owned parent.
    #[arg(long, default_value = "/var/lib/kbf-installer/orders", value_parser = kbf_host::HostPath::new)]
    pub orders_dir: kbf_host::HostPath,
    #[command(flatten)]
    pub service: ServiceArgs,
    /// The confirm window the boot check gives a release on trial (120 to 1800).
    #[arg(long, default_value_t = 900, value_parser = clap::value_parser!(u64).range(120..=1800))]
    pub window_secs: u64,
    /// The starts a release on trial gets before it is reverted (Linux).
    #[arg(long, default_value_t = 3)]
    pub starts: u32,
    /// The fewest seconds between one activation and the next.
    #[arg(long, default_value_t = 60)]
    pub min_activation_interval_secs: u64,
    /// Stop after this many passes (a rehearsal or a test); without it, run until stopped.
    #[arg(long)]
    pub passes: Option<u64>,
    /// installer.toml, under the host root: its `database` key is the state database the
    /// installer migrates to what a release needs before that release is activated. Required
    /// when the unit restarted after a swap is a frontend's or the all-in-one's.
    #[arg(long, value_parser = kbf_host::HostPath::new)]
    pub installer_config: Option<kbf_host::HostPath>,
}

#[derive(Args)]
pub struct MigrateArgs {
    /// Put the old layout back instead (refused once the installer has moved the host on).
    #[arg(long)]
    pub rollback: bool,
    /// A unit that runs kbf from `/usr/local/kbf/current`, to repoint at the installer's tree
    /// (repeat for several; `.service`, or `.plist` for launchd).
    #[arg(long = "unit", value_parser = crate::migrate::Unit::new)]
    pub units: Vec<crate::migrate::Unit>,
    /// The old helper's unit: stopped and disabled, so the installer is the only manager.
    #[arg(long, value_parser = crate::migrate::Unit::new)]
    pub helper_unit: Option<crate::migrate::Unit>,
    /// The installer's own unit, which a rollback stops and disables first.
    #[arg(long, value_parser = crate::migrate::Unit::new)]
    pub installer_unit: Option<crate::migrate::Unit>,
    /// Where the host gets its releases: decides where the replay floor is seeded from.
    #[arg(long, value_enum, default_value = "url")]
    pub release_source: ReleaseSource,
    /// The lowest generation an order may carry: the floor when there is no frontend to ask.
    /// Without it and without a frontend the host starts with no generation floor, and the
    /// run says so.
    #[arg(long)]
    pub min_generation: Option<u64>,
    /// The unprivileged user the old binaries' `--version --json` runs as.
    #[arg(long, default_value = "kbf-fetch")]
    pub fetch_user: String,
}

#[derive(Args)]
pub struct BootArgs {
    #[arg(long, default_value_t = 900, value_parser = clap::value_parser!(u64).range(120..=1800))]
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
            Verb::Rollback(_) => "rollback",
            Verb::Gc(_) => "gc",
            Verb::BootCheck(_) => "boot-check",
            Verb::Migrate(_) => "migrate",
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

    /// Whether the verb changes what a host runs or how it is managed, so it must not run
    /// while the old layout's state is the host's and unmigrated (the old helper and the
    /// installer never both manage one host; `migrate` is how that ends). `status`, `check`,
    /// `boot-check`, `quiesce` and `migrate` itself change no release.
    pub fn manages_host(&self) -> bool {
        !matches!(
            self,
            Verb::Status | Verb::Check | Verb::BootCheck(_) | Verb::Quiesce | Verb::Migrate(_)
        )
    }

    /// The pull request of the contract (section 9) that builds the verb, `None` for the
    /// verbs that exist (`run`, `boot-check`, the operator's verbs and `setup` for the modes with a database).
    pub fn built_by(&self) -> Option<&'static str> {
        match self {
            Verb::Run(_)
            | Verb::BootCheck(_)
            | Verb::Confirm(_)
            | Verb::Revert(_)
            | Verb::Rollback(_)
            | Verb::Gc(_)
            | Verb::Migrate(_)
            | Verb::Quiesce => None,
            // Only its database step is built; the rest of `setup` is not.
            Verb::Setup(a) if a.mode.keeps_state_in_database() => None,
            Verb::Setup(_) | Verb::Check => Some("P10"),
            Verb::Status => Some("P4"),
            Verb::Update(_) => Some("P13"),
            Verb::Activate => Some("P5"),
            Verb::Stage => Some("P8"),
            Verb::FetchChild => Some("P7"),
        }
    }
}
