//! `--state-store`: where the scheduler keeps its state (`docs/design/architecture-v2.md`,
//! section 4.3).

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use kbf_state::Store;

use crate::farm::{EpochPolicy, StateError};
use crate::members::MembershipConfig;

/// A `--state-store` value: Postgres (`docs/design/frontend-proxy.md`, section 11) or,
/// until it is deleted, SQLite.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateStoreUri {
    /// `sqlite:///absolute/path`: a database file on a local disk, owned by one process.
    Sqlite(PathBuf),
    /// `postgres:///kbf?host=/run/postgresql`: a database on a Unix socket, reached as the
    /// process's own user (peer authentication). Several frontends may open it at once and
    /// all write: none takes a generation, and the generation row holds a marker instead
    /// ([`StateStoreUri::open`]).
    Postgres(PostgresUrl),
}

/// The URL of a Postgres state store. It is not printed: `Debug` leaves it out, as a
/// URL may hold a password (the flag refuses one, see [`StateStoreParser`]; a value
/// built by [`FromStr`] may).
#[derive(Clone)]
pub struct PostgresUrl {
    text: String,
    // Boxed: a config is large next to the SQLite variant's path.
    config: Box<kbf_state::postgres::PostgresConfig>,
}

// The parsed config follows from the text, so the text decides.
impl PartialEq for PostgresUrl {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
    }
}

impl Eq for PostgresUrl {}

impl fmt::Debug for PostgresUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PostgresUrl(..)")
    }
}

impl PostgresUrl {
    /// The URL without user information or a `password` parameter: what two processes
    /// compare to know they mean the same database.
    fn identity(&self) -> String {
        let text = self.text.as_str();
        let (scheme, rest) = text.split_once("://").unwrap_or(("postgres", text));
        let (authority_and_path, query) = rest.split_once('?').map_or((rest, ""), |(a, q)| (a, q));
        let host_end = authority_and_path
            .find('/')
            .unwrap_or(authority_and_path.len());
        let (authority, path) = authority_and_path.split_at(host_end);
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        let query: Vec<&str> = query
            .split('&')
            .filter(|pair| !pair.is_empty())
            .filter(|pair| {
                let key = pair.split_once('=').map_or(*pair, |(key, _)| key);
                !key.eq_ignore_ascii_case("password")
            })
            .collect();
        let mut out = format!("{scheme}://{host}{path}");
        if !query.is_empty() {
            out.push('?');
            out.push_str(&query.join("&"));
        }
        out
    }
}

/// Why a `--state-store` value is refused. No message carries any part of the value: a
/// mistyped `postgres://` URL holds a password, and the command line is world-readable
/// and logged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StateStoreUriError {
    /// Any value that is not `sqlite://` followed by a path or a `postgres://` URL.
    #[error(
        "only postgres:///<database>?host=<socket directory> or sqlite:///<absolute path> is \
         supported"
    )]
    Unsupported,
    /// A `postgres://` or `postgresql://` value that is not a URL this client can use: it
    /// names no host, asks for TLS or does not parse.
    #[error(
        "postgres:// needs a URL with a host and no TLS, as in \
         postgres:///kbf?host=/run/postgresql"
    )]
    PostgresUrl,
    /// A `postgres://` value that carries a password. The command line is
    /// world-readable: the socket URL is reached by peer authentication and needs none.
    #[error(
        "postgres:// must carry no password (the command line is world-readable); use the \
         Unix socket with peer authentication, as in postgres:///kbf?host=/run/postgresql"
    )]
    PostgresPassword,
    /// A `sqlite://` value that does not name a file by an absolute path.
    #[error("sqlite:// needs an absolute file path, as in sqlite:///var/lib/kbf/state.db")]
    SqlitePath,
}

impl FromStr for StateStoreUri {
    type Err = StateStoreUriError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.starts_with("postgres://") || s.starts_with("postgresql://") {
            // The parser's own messages may quote the value; none is kept.
            let config = kbf_state::postgres::PostgresConfig::parse(s)
                .map_err(|_| StateStoreUriError::PostgresUrl)?
                .with_application_name("kbf-server");
            return Ok(Self::Postgres(PostgresUrl {
                text: s.to_owned(),
                config: Box::new(config),
            }));
        }
        let path = s
            .strip_prefix("sqlite://")
            .ok_or(StateStoreUriError::Unsupported)?;
        if path.starts_with('/') && path.len() > 1 {
            Ok(Self::Sqlite(PathBuf::from(path)))
        } else {
            Err(StateStoreUriError::SqlitePath)
        }
    }
}

/// The parser of `--state-store`. clap quotes a value back in the error of any parser it
/// wraps (`invalid value '<value>'`), and a value may hold a password, so this one builds
/// its own error, which is the fixed text of [`StateStoreUriError`].
#[derive(Clone, Copy)]
pub struct StateStoreParser;

impl clap::builder::TypedValueParser for StateStoreParser {
    type Value = StateStoreUri;

    fn parse_ref(
        &self,
        cmd: &clap::Command,
        _arg: Option<&clap::Arg>,
        value: &std::ffi::OsStr,
    ) -> Result<StateStoreUri, clap::Error> {
        let refuse = |why: StateStoreUriError| {
            cmd.clone().error(
                clap::error::ErrorKind::ValueValidation,
                format!("--state-store: {why}"),
            )
        };
        value
            .to_str()
            .ok_or(StateStoreUriError::Unsupported)
            .and_then(|text| text.parse::<StateStoreUri>())
            .and_then(|uri| match &uri {
                StateStoreUri::Postgres(url) if has_password(url) => {
                    Err(StateStoreUriError::PostgresPassword)
                }
                _ => Ok(uri),
            })
            .map_err(refuse)
    }
}

/// The Postgres store of a host that runs it beside kbf: the database and role `kbf` on
/// the OS package's socket directory, reached by peer authentication.
const LOCAL_POSTGRES: &str = "postgres:///kbf?host=/run/postgresql";

/// What a start on a `sqlite://` store logs, once: SQLite is on its way out.
const SQLITE_REMOVED: &str = "the SQLite state store is removed in the next release: move the \
     scheduler's state to Postgres before upgrading (see the upgrade notes)";

fn has_password(url: &PostgresUrl) -> bool {
    url.config.has_password()
}

impl StateStoreUri {
    /// What an [`open`](Self::open) does with the store's epoch: it keeps it, so a restart
    /// of a frontend, or the start of another one over the same Postgres, does not make
    /// the daemons drop the runs of the leases they hold. A fresh store has no epoch to
    /// keep: it is given a first one no other store's is likely to equal (`Durable::open`),
    /// and when several frontends open an empty store together they end with one.
    ///
    /// The epoch changes when an operator raises it (`POST /v1/epoch/bump`,
    /// [`Farm::bump_epoch`](crate::Farm::bump_epoch)), which a restore of the store from a
    /// backup calls for: the frontends that start after it serve the new epoch, and the
    /// daemons drop the leases a lost history granted.
    #[must_use]
    pub const fn epoch_policy(&self) -> EpochPolicy {
        match self {
            Self::Sqlite(_) | Self::Postgres(_) => EpochPolicy::Keep,
        }
    }

    /// The Postgres store on this host's own socket (`postgres:///kbf?host=/run/postgresql`):
    /// what `kbf all-in-one` opens when `--state-store` is left out.
    #[must_use]
    pub fn local_postgres() -> Self {
        LOCAL_POSTGRES
            .parse()
            .expect("LOCAL_POSTGRES is a socket URL the parser accepts")
    }

    /// What names the store, for two processes to agree they mean the same one: a SQLite
    /// store's file, a Postgres store's URL without a password.
    #[must_use]
    pub fn identity(&self) -> PathBuf {
        match self {
            Self::Sqlite(path) => path.clone(),
            Self::Postgres(url) => PathBuf::from(url.identity()),
        }
    }

    /// Opens the store as a frontend that shares it with others.
    ///
    /// A Postgres store takes no generation: any number of frontends open it and write,
    /// each conditionally on the rows it read (the transitions of `farm::durable`), and none
    /// fences another. What the open does write is the generation row, once: it ensures
    /// that `meta/generation` holds [`kbf_state::SHARED_WRITERS`], with a write conditional
    /// on the row read, so frontends that open together agree. A store that holds the marker
    /// opens as it is. A store that holds a generation (one that an older build which takes
    /// generations last opened) gets the marker in its place, which is the cold cutover: it
    /// is only right when every such frontend is stopped first, for a build that takes
    /// generations fails to start on the store, which holds a marker where it reads a
    /// number.
    ///
    /// A SQLite file is created, with its directory, if missing (directories it makes are
    /// 0700 and the file 0600: the scheduler's state is for this user alone), and held by
    /// this process alone: a second open of it (a second frontend) is refused. No generation
    /// is written to it.
    ///
    /// This blocks, as the store's calls do, and may be called from an async task.
    ///
    /// # Errors
    /// [`StateError::Open`]: the file is held by another process, is on a network
    /// filesystem or is not a state store; or the generation row of a Postgres store cannot
    /// be set to the marker.
    pub fn open(&self) -> Result<StateStore, StateError> {
        let store = match self {
            Self::Sqlite(path) => {
                tracing::warn!("{SQLITE_REMOVED}");
                open_sqlite(path)?
            }
            Self::Postgres(url) => {
                let store = open_postgres(url)?;
                kbf_state::share(store.as_ref()).map_err(open_error)?;
                store
            }
        };
        Ok(StateStore {
            store,
            epoch: self.epoch_policy(),
            // The defaults; a start sets the --member-* flags (`start::with_members`).
            members: MembershipConfig::default(),
            history: None,
        })
    }
}

impl crate::Args {
    /// The state store `--state-store` names, opened, or `None` when the flag is not
    /// given (the scheduler's state is then in memory).
    ///
    /// # Errors
    /// [`StateError`]: see [`StateStoreUri::open`].
    pub fn open_state_store(&self) -> Result<Option<StateStore>, StateError> {
        self.state_store
            .as_ref()
            .map(StateStoreUri::open)
            .transpose()
    }
}

fn open_error(why: impl fmt::Display) -> StateError {
    StateError::Open(why.to_string())
}

/// Connects, refuses a server that does not flush commits before it acknowledges them
/// (`synchronous_commit` is not on: `PostgresStore::open` checks every connection), and
/// creates the tables if they are not there. The errors name the setting or the failed
/// step, never the URL.
fn open_postgres(url: &PostgresUrl) -> Result<Arc<dyn Store>, StateError> {
    let store = kbf_state::postgres::PostgresStore::open(&url.config).map_err(open_error)?;
    Ok(Arc::new(store))
}

fn open_sqlite(path: &Path) -> Result<Arc<dyn Store>, StateError> {
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};

    use kbf_state::SystemClock;
    use kbf_state::sqlite::{Options, SqliteStore};
    // Only the root has no parent, and it exists: the open below then says what is wrong.
    let dir = path.parent().unwrap_or(path);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| open_error(format!("{}: {e}", dir.display())))?;
    // SQLite makes its file with the process's umask; the file is made here, 0600, and
    // the journal and WAL files SQLite makes beside it take its mode.
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| open_error(format!("{}: {e}", path.display())))?;
    let store = SqliteStore::open(path, Arc::new(SystemClock::default()), Options::default())
        .map_err(open_error)?;
    Ok(Arc::new(store))
}

/// An open state store, and what a start does with its epoch.
pub struct StateStore {
    /// The store.
    pub store: Arc<dyn Store>,
    /// The epoch the start takes.
    pub epoch: EpochPolicy,
    /// How long storage members live and dwell in the store ([`crate::members`]).
    pub members: MembershipConfig,
    /// Build history to write, if `--history-objects` names where: it keeps its index
    /// rows in this store ([`crate::history`]).
    pub history: Option<crate::history::History>,
}

#[cfg(test)]
mod tests;
