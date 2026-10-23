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
    Setup(Box<SetupArgs>),
    /// What is installed, staged and pending, and the last refusal.
    Status,
    /// Run by kbf-installer.service: act on the orders kbf wrote, revert an expired swap.
    Run(RunArgs),
    /// Keep the pending swap, naming its activation.
    Confirm(ActivationArgs),
    /// Undo the pending swap, naming its activation.
    Revert(ActivationArgs),
    /// Point `current` at `previous`, with no confirm window.
    Rollback(ServiceArgs),
    /// Record a release placed on the host by hand as its first
    ///
    /// Checks its files, writes its `VERIFIED` marker and makes it current, so the frontend
    /// proxy can start it. The operator vouches for the commit; nothing is fetched or attested.
    RecordRelease(RecordReleaseArgs),
    /// Remove the releases nothing needs: current, previous, staged, the swap on trial and any a
    /// process executes from stay, and so do the newest `--keep` others.
    Gc(GcArgs),
    /// Run before each start of a release on trial; counts the start.
    BootCheck(BootArgs),
    /// For maintainer scripts: take the lock; exit 5 while a swap is pending.
    Quiesce,
    /// Validate the installer's configuration, trust files and path chain.
    Check,
    /// Serve the frontend host's public port: route each stream to the active child frontend.
    Proxy(ProxyArgs),
    #[command(hide = true)]
    Stage,
    #[command(hide = true)]
    Activate,
    /// Internal: pull one component to standard output. The installer starts it itself, as the
    /// fetch user, with fixed flags; it is not a command for a person.
    #[command(hide = true)]
    FetchChild(crate::fetch::child::ChildArgs),
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
    Bundled,
}

#[derive(Args)]
pub struct SetupArgs {
    /// The kbf mode this host runs (`kbf --mode=...`).
    #[arg(long, value_enum)]
    pub mode: Mode,
    /// Where releases are fetched from. Default: the frontend for the modes that enroll,
    /// a file server (`url`) for `frontend` and `all-in-one`. `bundled` fetches nothing: it
    /// takes no --release-url and no --commit.
    #[arg(long, value_enum)]
    pub release_source: Option<ReleaseSource>,
    /// The https base URL of the file server, for --release-source url.
    #[arg(long)]
    pub release_url: Option<String>,
    /// The keys of the release source that go with it in installer.toml: the file server's CA
    /// (`url`) and the frontend's worker listener with its CA (`frontend`).
    #[command(flatten)]
    pub release: crate::setup::release_keys::ReleaseFlags,
    /// Refuse orders below this generation. A floor only goes up.
    #[arg(long)]
    pub min_generation: Option<u64>,
    /// The first release to install (40 hex characters); needs --release-source url.
    #[arg(long)]
    pub commit: Option<CommitId>,
    /// The frontend to enroll with, as https://host[:port]. For `storage`, `daemon` and
    /// `storage-and-daemon`; written to kbf.toml.
    #[arg(long)]
    pub frontend_url: Option<String>,
    /// The enrollment token (a file of mode 0600 or tighter); setup keeps a copy that only
    /// kbf reads. For the modes that enroll.
    #[arg(long)]
    pub token_file: Option<String>,
    /// The state database, as postgres:///NAME?host=SOCKET_DIR&user=ROLE (no password).
    /// Required for `frontend` and `all-in-one`: written to installer.toml and, as
    /// `state-store`, to kbf.toml, and setup creates (or brings forward) its schema. The
    /// server exists before setup runs, and so do the role and the database unless
    /// --create-local-database makes them.
    #[arg(long)]
    pub database: Option<String>,
    /// For a host whose Postgres runs locally: create the login role and the database the
    /// `--database` URL names (owned by the role, no password), as the server's superuser over
    /// its Unix socket (peer authentication, no password), if they are not there. Never
    /// edits `pg_hba.conf`; refuses, saying which rule to add, when the server does not admit
    /// the login. Without it, setup only checks that it can log in.
    #[arg(long)]
    pub create_local_database: bool,
    /// Enable the units without starting them.
    #[arg(long)]
    pub no_start: bool,
    /// The kbf.toml settings of a frontend or all-in-one host (listeners, certificates,
    /// storage): each sets the kbf flag of its name.
    #[command(flatten)]
    pub kbf: crate::setup::kbf_settings::KbfFlags,
    /// A frontend host behind `kbf-installer proxy`: the `[proxy]` table of installer.toml.
    #[command(flatten)]
    pub proxy: crate::setup::proxy::ProxyFlags,
    /// The installer.toml `setup` writes (and reads, to keep what it does not set), under the
    /// host root: `/etc/kbf/installer.toml`, `/usr/local/etc/kbf/installer.toml` on a Mac, which
    /// is the only value setup accepts. The same flag, with the same meaning, as on `run`.
    #[arg(long, value_parser = kbf_host::HostPath::new)]
    pub installer_config: Option<kbf_host::HostPath>,
}

#[derive(Args)]
pub struct ActivationArgs {
    /// The activation: `<epoch>-<order id>`.
    #[arg(long)]
    pub id: ActivationId,
    #[command(flatten)]
    pub service: ServiceArgs,
}

#[derive(Args)]
pub struct RecordReleaseArgs {
    /// The release to record (40 hex characters): `releases/<commit>/kbf` must be there.
    #[arg(long)]
    pub commit: CommitId,
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
    /// in the database (`kbf-frontend.service`, `kbf-frontend@<instance>.service`, `kbf-all-in-one.service` or its
    /// template `kbf-all-in-one@<instance>.service`): the installer is not told the host's
    /// mode, the unit it restarts is the one place that names it. A launchd label names no mode.
    pub fn runs_database_mode(&self) -> bool {
        self.unit.as_deref().is_some_and(|u| {
            u == "kbf-all-in-one.service"
                || u == "kbf-frontend.service"
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
    /// Default: the host's state directory (`/var/lib/kbf-installer`, `/var/db/kbf-installer` on a Mac).
    #[arg(long, value_parser = kbf_host::HostPath::new)]
    pub update_dir: Option<kbf_host::HostPath>,
    /// The order spool: owned by the service user, mode 0700, inside a root-owned parent.
    /// Default: `orders` in the state directory.
    #[arg(long, value_parser = kbf_host::HostPath::new)]
    pub orders_dir: Option<kbf_host::HostPath>,
    /// The systemd unit to restart after a swap (Linux). Without it and without
    /// `--launchd-label`, the unit of the mode in the host's kbf.toml, which is how the one
    /// packaged kbf-installer.service serves every mode.
    #[arg(long, conflicts_with = "launchd_label")]
    pub unit: Option<String>,
    /// The launchd label to kickstart after a swap (macOS).
    #[arg(long)]
    pub launchd_label: Option<String>,
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
    /// when the unit restarted after a swap is a frontend's or the all-in-one's, unless the
    /// unit comes from kbf.toml's mode (no --unit): then the host's installer.toml (/etc/kbf, /usr/local/etc/kbf on a Mac) is the default.
    #[arg(long, value_parser = kbf_host::HostPath::new)]
    pub installer_config: Option<kbf_host::HostPath>,
}

impl RunArgs {
    /// Root's directory, as the flag gives it; else the host's own.
    pub fn update_dir_in(&self, layout: &crate::layout::Layout) -> kbf_host::HostPath {
        self.update_dir
            .clone()
            .unwrap_or_else(|| layout.update_dir())
    }

    /// The order spool, as the flag gives it; else the host's own.
    pub fn orders_dir_in(&self, layout: &crate::layout::Layout) -> kbf_host::HostPath {
        self.orders_dir
            .clone()
            .unwrap_or_else(|| layout.orders_dir())
    }

    /// The service this `run` restarts, when the flags name it; `None` when neither does.
    pub fn named_service(&self) -> Option<ServiceArgs> {
        (self.unit.is_some() || self.launchd_label.is_some()).then(|| ServiceArgs {
            unit: self.unit.clone(),
            launchd_label: self.launchd_label.clone(),
        })
    }
}

#[derive(Args)]
pub struct ProxyArgs {
    /// The installer.toml whose `[proxy]` table configures the proxy (default: the platform's,
    /// `/etc/kbf/installer.toml`, `/usr/local/etc/kbf/installer.toml` on a Mac).
    #[arg(long)]
    pub config: Option<std::path::PathBuf>,
    /// The order spool the update API writes and the root half reads: owned by the service
    /// user. As `run --orders-dir`. Default: `orders` in the host's state directory.
    #[arg(long)]
    pub orders_dir: Option<std::path::PathBuf>,
    /// Check the `[proxy]` table and exit: 0 if it is valid, 2 if not. Binds nothing.
    #[arg(long)]
    pub check: bool,
    /// Root's directory: the proxy reads `status.json` here, to start the installed release
    /// when no frontend child is running, and the update API reads its status. As
    /// `run --update-dir`. Default: the host's state directory (`/var/lib/kbf-installer`,
    /// `/var/db/kbf-installer` on a Mac).
    #[arg(long)]
    pub update_dir: Option<std::path::PathBuf>,
    /// The kbf the package installed (`/usr/bin/kbf`; an absolute path). With it the proxy keeps
    /// a verified copy of each kbf it runs in `releases` of its state directory, starts its
    /// children only from those copies, and keeps its installed and staged releases in
    /// `releases.json` there: root's `status.json` and order spool are not used (`--update-dir`
    /// and `--orders-dir` are ignored) and the update API orders nothing. Without it the proxy
    /// works as before.
    #[arg(long)]
    pub kbf_binary: Option<std::path::PathBuf>,
}

impl ProxyArgs {
    /// Root's directory, as the flag gives it; else the host's own (as [`RunArgs::update_dir_in`]).
    pub fn update_dir_in(&self, layout: &crate::layout::Layout) -> std::path::PathBuf {
        self.update_dir
            .clone()
            .unwrap_or_else(|| layout.update_dir().as_str().into())
    }

    /// The order spool, as the flag gives it; else the host's own.
    pub fn orders_dir_in(&self, layout: &crate::layout::Layout) -> std::path::PathBuf {
        self.orders_dir
            .clone()
            .unwrap_or_else(|| layout.orders_dir().as_str().into())
    }
}

#[derive(Args)]
pub struct BootArgs {
    #[arg(long, default_value_t = 900, value_parser = clap::value_parser!(u64).range(120..=1800))]
    pub window_secs: u64,
    /// The kbf this unit starts, on a bundled host (one with no releases and no state): the
    /// check runs `<kbf> boot-check --mode=<mode>` over the host's installer.toml (under
    /// --root), which migrates the state database, and ends as it ends. Needs --mode.
    #[arg(long, requires = "mode")]
    pub kbf: Option<std::path::PathBuf>,
    /// The mode of the unit that runs the check: only frontend and all-in-one keep state in
    /// the database. Needs --kbf.
    #[arg(long, value_enum, requires = "kbf")]
    pub mode: Option<Mode>,
}

impl BootArgs {
    /// The kbf and the mode of a bundled host's unit, when the unit gave them (clap makes
    /// the two come together or not at all).
    pub fn bundled(&self) -> Option<(&std::path::Path, Mode)> {
        self.kbf.as_deref().zip(self.mode)
    }
}

impl Verb {
    /// The verb as typed.
    pub fn name(&self) -> &'static str {
        match self {
            Verb::Setup(_) => "setup",
            Verb::Status => "status",
            Verb::Run(_) => "run",
            Verb::Confirm(_) => "confirm",
            Verb::Revert(_) => "revert",
            Verb::Rollback(_) => "rollback",
            Verb::RecordRelease(_) => "record-release",
            Verb::Gc(_) => "gc",
            Verb::BootCheck(_) => "boot-check",
            Verb::Quiesce => "quiesce",
            Verb::Check => "check",
            Verb::Proxy(_) => "proxy",
            Verb::Stage => "stage",
            Verb::Activate => "activate",
            Verb::FetchChild(_) => "fetch-child",
        }
    }

    /// Every verb but `status`, `check` and `proxy` (which runs as the unprivileged service
    /// user) needs root.
    pub fn needs_root(&self) -> bool {
        !matches!(self, Verb::Status | Verb::Check | Verb::Proxy(_))
    }

    /// The pull request of the contract (section 9) that builds the verb, `None` for the
    /// verbs that exist (`run`, `boot-check`, `proxy`, the operator's verbs and
    /// `setup`).
    pub fn built_by(&self) -> Option<&'static str> {
        match self {
            Verb::Run(_)
            | Verb::BootCheck(_)
            | Verb::Proxy(_)
            | Verb::Confirm(_)
            | Verb::Revert(_)
            | Verb::Rollback(_)
            | Verb::RecordRelease(_)
            | Verb::Gc(_)
            | Verb::Quiesce
            | Verb::Setup(_)
            | Verb::FetchChild(_) => None,
            Verb::Check => Some("P10"),
            Verb::Status => Some("P4"),
            Verb::Activate => Some("P5"),
            Verb::Stage => Some("P8"),
        }
    }
}
