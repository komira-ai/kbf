//! The flags of `kbf-installer setup` and `kbf setup`, and the modes they set a host up for. The
//! rest of the installer's command line is in `kbf_installer::cli`, which re-exports these two
//! and adds the proxy's flags (`--proxy` and the ones that go with it, which `setup` refuses) to
//! these: they are not here, so that the `kbf` binary's `setup` takes none of them whatever
//! features are built.

use clap::{Args, ValueEnum};

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

#[derive(Args)]
pub struct SetupArgs {
    /// The kbf mode this host runs (`kbf --mode=...`).
    #[arg(long, value_enum)]
    pub mode: Mode,
    /// Refuse orders below this generation. A floor only goes up.
    #[arg(long)]
    pub min_generation: Option<u64>,
    /// The frontend to enroll with, as https://host[:port]. For `storage`, `daemon` and
    /// `storage-and-daemon`; written to kbf.toml.
    #[arg(long)]
    pub frontend_url: Option<String>,
    /// The enrollment token (a file of mode 0600 or tighter); setup keeps a copy that only
    /// kbf reads. For the modes that enroll.
    #[arg(long)]
    pub token_file: Option<String>,
    /// The state database, as postgres:///NAME?host=SOCKET_DIR&user=ROLE (no password), for
    /// `frontend` and `all-in-one`: written to installer.toml and, as `state-store`, to kbf.toml,
    /// and setup creates (or brings forward) its schema. Left out, it is
    /// postgres:///kbf?host=/run/postgresql&user=ROLE on this host's own Postgres server (ROLE is
    /// kbf-server for `frontend`, kbf for `all-in-one`: the account the mode's unit runs as), and
    /// setup creates the role and the database as if --create-local-database were given.
    /// Given, the server exists before setup runs, and so do the role and the database unless
    /// --create-local-database makes them.
    #[arg(long)]
    pub database: Option<String>,
    /// Create the login role and the database the `--database` URL names (owned by the role,
    /// no password), on a host whose Postgres runs locally, as the server's superuser over its
    /// Unix socket (peer authentication, no password), if they are not there. `frontend` and
    /// `all-in-one` do this whenever --database is left out, so the flag only matters beside
    /// --database. Never edits `pg_hba.conf`; prints the rules the logins need, and refuses,
    /// saying which rule to add, when the server does not admit a login. With --database and
    /// without this flag, setup only checks that it can log in.
    #[arg(long)]
    pub create_local_database: bool,
    /// Enable the units without starting them.
    #[arg(long)]
    pub no_start: bool,
    /// The kbf.toml settings of a frontend or all-in-one host (listeners, certificates,
    /// storage): each sets the kbf flag of its name.
    #[command(flatten)]
    pub kbf: crate::setup::kbf_settings::KbfFlags,
    /// The installer.toml `setup` writes (and reads, to keep what it does not set), under the
    /// host root: `/etc/kbf/installer.toml`, `/usr/local/etc/kbf/installer.toml` on a Mac, which
    /// is the only value setup accepts. The same flag, with the same meaning, as on `run`.
    #[arg(long, value_parser = kbf_host::HostPath::new)]
    pub installer_config: Option<kbf_host::HostPath>,
    /// The flags that no longer do anything: the release source and its keys, and the first
    /// release. Taken and ignored for one release, with a warning.
    #[command(flatten)]
    pub retired: crate::setup::retired::RetiredFlags,
}
