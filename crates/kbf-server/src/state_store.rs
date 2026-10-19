//! `--state-store`: where the scheduler keeps its state (`docs/design/architecture-v2.md`,
//! section 4.3).

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use kbf_state::{
    Changes, Commit, CommitStats, Conflict, Fenced, GENERATION, Revision, ScanPage, ScanRange,
    Store, StoreError, StoreTime, Txn, Versioned,
};

use crate::farm::{EpochPolicy, StateError};
use crate::members::MembershipConfig;

/// A `--state-store` value: Postgres (`docs/design/frontend-proxy.md`, section 11) or,
/// until it is deleted, SQLite.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateStoreUri {
    /// `sqlite:///absolute/path`: a database file on a local disk, owned by one process.
    Sqlite(PathBuf),
    /// `postgres:///kbf?host=/run/postgresql`: a database on a Unix socket, reached as the
    /// process's own user (peer authentication). Several processes may open it; the
    /// generation fence ([`Fenced`]) lets one write at a time.
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

fn has_password(url: &PostgresUrl) -> bool {
    url.config.has_password()
}

impl StateStoreUri {
    /// What an [`open`](Self::open) does with the store's epoch: a new one. An open is a
    /// cold start (no live holder hands the store over; that is
    /// [`open_after_handover`](Self::open_after_handover), which keeps it), so a restored
    /// or swapped file, or a Postgres database restored from a backup, can never resume
    /// leases an earlier incarnation granted. A fresh store has no epoch to bump: it is
    /// given a first one no other store's is likely to equal (`Durable::open`).
    #[must_use]
    pub const fn epoch_policy(&self) -> EpochPolicy {
        match self {
            Self::Sqlite(_) | Self::Postgres(_) => EpochPolicy::Bump,
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

    /// What names the store, for two processes to agree they mean the same one (the
    /// handover): a SQLite store's file, a Postgres store's URL without a password.
    #[must_use]
    pub fn identity(&self) -> PathBuf {
        match self {
            Self::Sqlite(path) => path.clone(),
            Self::Postgres(url) => PathBuf::from(url.identity()),
        }
    }

    /// Opens the store. The file is created, with its directory, if missing (directories
    /// it makes are 0700 and the file 0600: the scheduler's state is for this user alone),
    /// and held by this process alone: a second open of it (a second frontend) is
    /// refused.
    ///
    /// This blocks, as the store's calls do, and may be called from an async task.
    ///
    /// # Errors
    /// [`StateError::Open`]: the file is held by another process, is on a network
    /// filesystem or is not a state store.
    pub fn open(&self) -> Result<StateStore, StateError> {
        self.open_at(None, self.epoch_policy())
    }

    /// Opens the store as the successor of the process that held generation `generation`
    /// and has let go of it, and keeps its epoch: the daemons that reconnect keep the runs
    /// of the leases they hold. Nothing is written unless the store is at exactly that
    /// generation, so a file that was restored or swapped since is refused, and so is one
    /// another writer has taken.
    ///
    /// # Errors
    /// [`StateError::Open`]: as [`StateStoreUri::open`], or the store is not at
    /// `generation`.
    pub fn open_after_handover(&self, generation: u64) -> Result<StateStore, StateError> {
        self.open_at(Some(generation), EpochPolicy::Keep)
    }

    /// Opens the store to wait for a takeover: the generation is not taken, the epoch is
    /// not touched and nothing is written ([`Candidate`]). The backend's own open still
    /// refuses a store whose schema version this build does not support; no schema is
    /// made or changed here.
    ///
    /// # Errors
    /// [`StateError::Open`]: as [`StateStoreUri::open`].
    pub fn open_candidate(&self) -> Result<Candidate, StateError> {
        Ok(Candidate::over(self.open_backend()?))
    }

    /// The backend's store, opened as it is: no generation taken, nothing written here.
    fn open_backend(&self) -> Result<Arc<dyn Store>, StateError> {
        match self {
            Self::Sqlite(path) => open_sqlite(path),
            Self::Postgres(url) => open_postgres(url),
        }
    }

    fn open_at(&self, expect: Option<u64>, epoch: EpochPolicy) -> Result<StateStore, StateError> {
        let inner = self.open_backend()?;
        // This process is the store's writer from here: it takes the next generation, and
        // every write through the handle is checked against it ([`Fenced`]).
        let fenced = Fenced::take(inner, expect).map_err(open_error)?;
        Ok(StateStore {
            store: Arc::new(fenced),
            epoch,
            // The defaults; a start sets the --member-* flags (`start::with_members`).
            members: MembershipConfig::default(),
            history: None,
        })
    }
}

/// A store opened by a process that waits to take over from the one that holds the
/// generation: it can read, and no write goes through the handle it gives out. It takes
/// the generation only when [`Candidate::take_over`] is called, expecting the holder's.
pub struct Candidate {
    inner: Arc<dyn Store>,
}

impl Candidate {
    /// A candidate over an open store (what [`StateStoreUri::open_candidate`] makes of a
    /// backend's store, and what a test makes of an in-memory one).
    #[must_use]
    pub fn over(inner: Arc<dyn Store>) -> Self {
        Self { inner }
    }

    /// A handle that reads the store and refuses every write.
    #[must_use]
    pub fn reader(&self) -> Arc<dyn Store> {
        Arc::new(ReadOnly(Arc::clone(&self.inner)))
    }

    /// The generation the store is at now (0 when no writer has taken one).
    ///
    /// # Errors
    /// [`StateError::Store`]: the store cannot be read, or the row is not a number.
    pub fn generation(&self) -> Result<u64, StateError> {
        generation_of(self.inner.as_ref())
    }

    /// Takes the generation, which must be exactly `expect`, and keeps the epoch: the
    /// daemons that reconnect keep the runs of their leases. Nothing is written when the
    /// store is at another generation. Returns the store the caller now writes through.
    ///
    /// # Errors
    /// [`StateError::Open`]: the store is not at `expect`, or another writer took the
    /// generation first.
    pub fn take_over(&self, expect: u64) -> Result<StateStore, StateError> {
        let fenced = Fenced::take(Arc::clone(&self.inner), Some(expect)).map_err(open_error)?;
        Ok(StateStore {
            store: Arc::new(fenced),
            epoch: EpochPolicy::Keep,
            members: MembershipConfig::default(),
            history: None,
        })
    }
}

/// The generation row of `store`: 0 when it has none.
///
/// # Errors
/// [`StateError::Store`] when the store cannot be read; [`StateError::Corrupt`] when the
/// row is not eight bytes.
pub fn generation_of(store: &dyn Store) -> Result<u64, StateError> {
    let Some(row) = store.get(GENERATION)? else {
        return Ok(0);
    };
    let bytes = <[u8; 8]>::try_from(row.value.as_slice()).map_err(|_| StateError::Corrupt {
        key: GENERATION.to_owned(),
        why: "not an 8-byte number".to_owned(),
    })?;
    Ok(u64::from_be_bytes(bytes))
}

/// A store handle that reads and writes nothing: what a candidate probes.
struct ReadOnly(Arc<dyn Store>);

impl Store for ReadOnly {
    fn apply_batch(&self, _: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        Err(StoreError::Failed(
            "this process waits for a takeover and writes nothing".to_owned(),
        ))
    }
    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.0.get(key)
    }
    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.0.scan_prefix(prefix)
    }
    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.0.scan_range(scan)
    }
    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.0.changes(after, limit)
    }
    fn reap_expired(&self) -> Result<usize, StoreError> {
        Err(StoreError::Failed(
            "this process waits for a takeover and writes nothing".to_owned(),
        ))
    }
    fn commit_stats(&self) -> CommitStats {
        self.0.commit_stats()
    }
    fn now(&self) -> StoreTime {
        self.0.now()
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
