//! The flags of `setup`, checked before anything is read from or written to the host.
//! Every problem is reported, one line each, not just the first.
//!
//! The rules follow from which modes enroll (docs/design/kbf-installer.md section 3): the
//! node modes `storage`, `daemon` and `storage-and-daemon` enroll with a frontend, so they
//! need its URL and an enrollment token; the `frontend` and `all-in-one` hosts do not enroll,
//! so they take neither. The flags that named a release source or a first release are not
//! checked: they are taken and ignored ([`RetiredFlags`]).

use super::kbf_settings::{KbfFlags, Setting};
use super::paths;
use super::plan::database_user_name;
#[cfg(feature = "proxy")]
use super::proxy::{ProxyFlags, ProxyPlan};
use super::retired::RetiredFlags;
use crate::cli::Mode;
use crate::installer_toml::https_authority_named;
use crate::layout::Platform;
use kbf_state::postgres::PostgresConfig;

/// The flags as the command line gave them.
#[derive(Clone, Debug)]
pub struct Raw {
    pub mode: Mode,
    pub frontend_url: Option<String>,
    pub token_file: Option<String>,
    pub min_generation: Option<u64>,
    /// The flags that no longer do anything; `setup` warns when any was given.
    pub retired: RetiredFlags,
    /// `--database`: the state database's URL.
    pub database: Option<String>,
    /// The listener, certificate and storage-client flags of a frontend or all-in-one host.
    pub kbf: KbfFlags,
    /// `--proxy` and what goes with it. Only a build with the `proxy` feature (kbf-installer's)
    /// has them.
    #[cfg(feature = "proxy")]
    pub proxy: ProxyFlags,
    /// `--create-local-database`.
    pub create_local_database: bool,
    /// `--installer-config`, as the host path it names; `None` is the platform's default.
    pub installer_config: Option<String>,
}

/// The flags, valid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Valid {
    pub mode: Mode,
    /// `https://host[:port]` of the frontend, for the node modes.
    pub frontend_url: Option<String>,
    /// The operator's enrollment token file, an absolute path, for the node modes.
    pub token_file: Option<String>,
    /// The operator floor the flag gives; the file's own is kept when there is none.
    pub min_generation: Option<u64>,
    /// The state database's `postgres://` URL, for the modes that keep their state there. It
    /// has no password and no character a TOML string would escape. No message quotes it.
    pub database: Option<String>,
    /// Whether `database` is the local default because `--database` was not given.
    pub database_defaulted: bool,
    /// The `kbf.toml` settings the listener, certificate and storage flags give.
    pub kbf: Vec<Setting>,
    /// The proxy host's settings: `Some` for `--proxy`. The files it names are not in `kbf`.
    #[cfg(feature = "proxy")]
    pub proxy: Option<ProxyPlan>,
    /// With `--create-local-database`, or no `--database` on a mode that keeps state in Postgres:
    /// what to create, from the URL.
    pub local_database: Option<LocalDatabase>,
}

impl Valid {
    /// The loopback port of the `kbf-storage-and-daemon` sibling an all-in-one host runs; none
    /// on every other host. Its presence is what makes a host the new all-in-one layout.
    #[cfg(feature = "proxy")]
    pub fn storage_port(&self) -> Option<u16> {
        self.proxy.as_ref().and_then(|plan| plan.storage_port)
    }

    /// Without the `proxy` feature no host has a sibling: all-in-one is refused.
    #[cfg(not(feature = "proxy"))]
    pub fn storage_port(&self) -> Option<u16> {
        None
    }

    /// Whether the host is set up behind the proxy (`--proxy`).
    #[cfg(feature = "proxy")]
    pub fn behind_proxy(&self) -> bool {
        self.proxy.is_some()
    }

    /// Without the `proxy` feature no host is set up behind the proxy.
    #[cfg(not(feature = "proxy"))]
    pub fn behind_proxy(&self) -> bool {
        false
    }

    /// The settings that name files the proxy reads, for `check_files`: none unless the host is
    /// set up behind the proxy.
    #[cfg(feature = "proxy")]
    pub fn proxy_files(&self) -> Vec<Setting> {
        self.proxy.as_ref().map_or_else(Vec::new, ProxyPlan::files)
    }

    /// Without the `proxy` feature the proxy reads no file.
    #[cfg(not(feature = "proxy"))]
    pub fn proxy_files(&self) -> Vec<Setting> {
        Vec::new()
    }
}

/// The role and database `--create-local-database` makes, and where the local server's socket
/// is, all from the `--database` URL. The names are plain (letters, digits, `_`, `-`).
#[derive(Clone, PartialEq, Eq)]
pub struct LocalDatabase {
    pub role: String,
    pub name: String,
    /// The Unix socket directory (`host=` of the URL), absolute.
    pub socket_dir: String,
    pub port: Option<u16>,
}

impl std::fmt::Debug for LocalDatabase {
    // The names come from the operator's URL; no message or log carries them.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalDatabase").finish_non_exhaustive()
    }
}

/// Whether the mode enrolls with a frontend: its start does, with the token.
pub fn enrolls(mode: Mode) -> bool {
    matches!(mode, Mode::Storage | Mode::Daemon | Mode::StorageAndDaemon)
}

impl Raw {
    /// Whether `--proxy` was given.
    #[cfg(feature = "proxy")]
    pub fn behind_proxy(&self) -> bool {
        self.proxy.proxy
    }

    /// Without the `proxy` feature there is no `--proxy`.
    #[cfg(not(feature = "proxy"))]
    pub fn behind_proxy(&self) -> bool {
        false
    }

    /// An all-in-one host is always behind the proxy, which a build without the `proxy` feature
    /// does not set up: the problem is said once, for the program that has no `--proxy`.
    #[cfg(not(feature = "proxy"))]
    fn check_all_in_one(&self, problems: &mut Vec<String>) {
        if self.mode == Mode::AllInOne {
            problems.push(
                "--mode=all-in-one runs behind the proxy, which this setup does not take: use kbf-installer setup with --proxy".to_owned(),
            );
        }
    }

    /// Check every flag; the `Err` lists each problem.
    pub fn validate(&self, platform: Platform) -> Result<Valid, Vec<String>> {
        let mut problems = Vec::new();
        let node = enrolls(self.mode);
        let name = mode_name(self.mode);

        let frontend_url = match (&self.frontend_url, node) {
            (Some(u), true) => https_authority_named("--frontend-url", u)
                .map_err(|e| problems.push(e))
                .ok(),
            (None, true) => {
                problems.push(format!("--mode={name} enrolls: --frontend-url is required"));
                None
            }
            (Some(_), false) => {
                problems.push(format!(
                    "--frontend-url is for the modes that enroll; --mode={name} does not"
                ));
                None
            }
            (None, false) => None,
        };
        let token_file = match (&self.token_file, node) {
            (Some(t), true) if t.starts_with('/') => Some(t.clone()),
            (Some(t), true) => {
                problems.push(format!("--token-file {t:?} must be an absolute path"));
                None
            }
            (None, true) => {
                problems.push(format!("--mode={name} enrolls: --token-file is required"));
                None
            }
            (Some(_), false) => {
                problems.push(format!(
                    "--token-file is for the modes that enroll; --mode={name} does not"
                ));
                None
            }
            (None, false) => None,
        };

        let (database, local_database) = self.database(&mut problems);
        let database_defaulted = database.is_some() && self.database.is_none();
        #[cfg(feature = "proxy")]
        let mut kbf = self
            .kbf
            .validate(self.mode, self.behind_proxy(), &mut problems);
        #[cfg(feature = "proxy")]
        let proxy = self
            .proxy
            .validate(platform, self.mode, &self.kbf, &mut kbf, &mut problems);
        #[cfg(not(feature = "proxy"))]
        let kbf = self
            .kbf
            .validate(self.mode, self.behind_proxy(), &mut problems);
        #[cfg(not(feature = "proxy"))]
        self.check_all_in_one(&mut problems);
        let installer_toml = paths::installer_toml(platform);
        if self
            .installer_config
            .as_deref()
            .is_some_and(|given| given != installer_toml)
        {
            problems.push(format!(
                "setup writes {installer_toml}, where the installer's unit reads it: --installer-config is for run"
            ));
        }
        if self.min_generation == Some(0) {
            problems.push("--min-generation 0 is no floor: leave it out".to_owned());
        }
        if !problems.is_empty() {
            return Err(problems);
        }
        Ok(Valid {
            mode: self.mode,
            frontend_url,
            token_file,
            min_generation: self.min_generation,
            database,
            database_defaulted,
            kbf,
            #[cfg(feature = "proxy")]
            proxy,
            local_database,
        })
    }

    /// The state database: used where the mode keeps its state in one, refused elsewhere.
    /// Without `--database` such a mode gets the local default ([`default_database`]) and
    /// has the role and database made, as `--create-local-database` does. The file the URL
    /// goes into is world-readable, so a password is refused, and the text of a URL is never
    /// part of a message. The URL names the role (`user=`): setup runs as root and the
    /// service as its own account, so a role left to the process's user would be two
    /// different ones. When the role and database are made it also names a database and a
    /// Unix socket directory, and the names are plain.
    fn database(&self, problems: &mut Vec<String>) -> (Option<String>, Option<LocalDatabase>) {
        let name = mode_name(self.mode);
        if !self.mode.keeps_state_in_database() {
            if self.database.is_some() {
                problems.push(format!(
                    "--database is for the modes that keep their state in Postgres; --mode={name} keeps none"
                ));
            }
            if self.create_local_database {
                problems.push(format!(
                    "--create-local-database is for the modes that keep their state in Postgres; --mode={name} keeps none"
                ));
            }
            return (None, None);
        }
        let default = default_database(self.mode);
        let url = self.database.as_ref().unwrap_or(&default);
        let create = self.create_local_database || self.database.is_none();
        let plain = url
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\');
        let postgres = url.starts_with("postgres://") || url.starts_with("postgresql://");
        let parsed = PostgresConfig::parse(url);
        let config = match (plain && postgres, parsed) {
            (true, Ok(config)) if !config.has_password() => config,
            (true, Ok(_)) => {
                problems.push(
                    "--database carries a password, and installer.toml is world-readable: use a Unix socket and peer authentication".to_owned(),
                );
                return (None, None);
            }
            _ => {
                problems.push(
                    "--database is not a usable Postgres URL of plain characters, as in postgres:///kbf?host=/run/postgresql&user=kbf".to_owned(),
                );
                return (None, None);
            }
        };
        if config.user().is_none() {
            problems.push(
                "--database names no role: add user=ROLE (setup runs as root and the service as its own account, so a role left to the process's user would differ)".to_owned(),
            );
            return (None, None);
        }
        if config.host_count() > 1 || config.port_count() > 1 {
            problems.push(
                "--database must name one server: with several hosts or ports, setup would make the role on the first and the service could log in to another".to_owned(),
            );
            return (None, None);
        }
        let local = if create {
            local_database(&config, problems)
        } else {
            None
        };
        (Some(url.clone()), local)
    }
}

/// The state database of a host that gives no `--database`: the database `kbf` on this host's
/// own Postgres socket, reached as the account the frontend runs as.
fn default_database(mode: Mode) -> String {
    format!(
        "postgres:///kbf?host=/run/postgresql&user={}",
        database_user_name(mode)
    )
}

/// What `--create-local-database` makes from the URL, or why it cannot. Only a Unix socket
/// directory is local, and only plain names are made: quoted identifiers would take anything,
/// but a name the operator mistyped should be refused, not created.
fn local_database(config: &PostgresConfig, problems: &mut Vec<String>) -> Option<LocalDatabase> {
    let socket_dir = config
        .socket_dir()
        .and_then(|d| d.to_str())
        .filter(|d| d.starts_with('/'));
    let role = config
        .user()
        .filter(|r| plain_name(r) && *r != "postgres" && !r.starts_with("pg_"));
    let name = config.dbname().filter(|n| plain_name(n));
    if socket_dir.is_none() {
        problems.push(
            "--create-local-database needs --database to name the server's Unix socket directory (host=/run/postgresql): setup reaches the server's superuser only over a local socket, so a remote server is the operator's to prepare".to_owned(),
        );
    }
    if role.is_none() {
        problems.push(
            "--create-local-database: the role in --database must be 1 to 63 characters of letters, digits, _ and -, and neither `postgres` nor `pg_`-prefixed".to_owned(),
        );
    }
    if name.is_none() {
        problems.push(
            "--create-local-database: --database must name a database of 1 to 63 characters of letters, digits, _ and -".to_owned(),
        );
    }
    Some(LocalDatabase {
        role: role?.to_owned(),
        name: name?.to_owned(),
        socket_dir: socket_dir?.to_owned(),
        port: config.port(),
    })
}

/// A name setup will create: letters, digits, `_` and `-`, at most 63 bytes (Postgres's
/// limit), not starting with `-`.
fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Frontend => "frontend",
        Mode::Storage => "storage",
        Mode::Daemon => "daemon",
        Mode::StorageAndDaemon => "storage-and-daemon",
        Mode::AllInOne => "all-in-one",
    }
}
