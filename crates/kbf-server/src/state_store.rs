//! `--state-store`: where the scheduler keeps its state (`docs/design/architecture-v2.md`,
//! section 4.3).

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use kbf_state::Store;

use crate::farm::{EpochPolicy, StateError};

/// A `--state-store` value: one of the two backends the design admits.
#[derive(Clone, PartialEq, Eq)]
pub enum StateStoreUri {
    /// `sqlite:///absolute/path`: a database file on a local disk, owned by one process.
    Sqlite(PathBuf),
    /// `postgres://...`, kept whole: it names a host and may carry a password, so it is
    /// never printed.
    Postgres(String),
}

/// Why a `--state-store` value is refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StateStoreUriError {
    /// The scheme is neither of the two.
    #[error("scheme {0:?} is not sqlite:///absolute/path or postgres://...")]
    Scheme(String),
    /// A `sqlite://` value that does not name a file by an absolute path.
    #[error("sqlite:// needs an absolute file path, as in sqlite:///var/lib/kbf/state.db")]
    SqlitePath,
    /// A `postgres://` value with nothing after the scheme.
    #[error("postgres:// names no server or database")]
    PostgresEmpty,
    /// A `postgres://` value that carries a password. The command line is readable by
    /// every user of the machine, so the password goes in a file instead.
    #[error(
        "the postgres:// value carries a password, and the command line is world-readable; \
         leave it out and give --state-store-password-file"
    )]
    PostgresPassword,
}

impl FromStr for StateStoreUri {
    type Err = StateStoreUriError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(path) = s.strip_prefix("sqlite://") {
            return if path.starts_with('/') && path.len() > 1 {
                Ok(Self::Sqlite(PathBuf::from(path)))
            } else {
                Err(StateStoreUriError::SqlitePath)
            };
        }
        if let Some(rest) = s.strip_prefix("postgres://") {
            return if rest.is_empty() {
                Err(StateStoreUriError::PostgresEmpty)
            } else if carries_password(rest) {
                Err(StateStoreUriError::PostgresPassword)
            } else {
                Ok(Self::Postgres(s.to_owned()))
            };
        }
        // Only the scheme is echoed: a mistyped postgres value may carry a password.
        let scheme = s.split_once("://").map_or(s, |(scheme, _)| scheme);
        Err(StateStoreUriError::Scheme(scheme.to_owned()))
    }
}

/// Whether the part of a `postgres://` URL after the scheme holds a password: in the
/// user information (`user:password@host`) or as a `password=` parameter.
fn carries_password(rest: &str) -> bool {
    let (authority, query) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
    let in_userinfo = authority
        .rsplit_once('@')
        .is_some_and(|(userinfo, _)| userinfo.contains(':'));
    in_userinfo || query.contains("password=")
}

impl fmt::Debug for StateStoreUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(path) => f.debug_tuple("Sqlite").field(path).finish(),
            Self::Postgres(_) => f.write_str("Postgres(..)"),
        }
    }
}

impl StateStoreUri {
    /// The scheme, as the value spells it.
    #[must_use]
    pub const fn scheme(&self) -> &'static str {
        match self {
            Self::Sqlite(_) => "sqlite",
            Self::Postgres(_) => "postgres",
        }
    }

    /// What a start does with the store's epoch. A SQLite file is the process's own, so
    /// every start is a new incarnation (a restored or swapped file can never resume
    /// leases an earlier one granted); a Postgres database outlives the process, so a
    /// start joins its incarnation and only a restore or failover raises the epoch.
    #[must_use]
    pub const fn epoch_policy(&self) -> EpochPolicy {
        match self {
            Self::Sqlite(_) => EpochPolicy::Bump,
            Self::Postgres(_) => EpochPolicy::Keep,
        }
    }

    /// Opens the store. A SQLite file is created, with its directory, if missing, and
    /// held by this process alone: a second open of it (a second frontend) is refused.
    /// `password` is the Postgres password, read from `--state-store-password-file`.
    ///
    /// This blocks, as the store's calls do, and may be called from an async task.
    ///
    /// # Errors
    /// [`StateError::NoBackend`]: a `postgres://` value in a build without the `postgres`
    /// feature. [`StateError::Open`]: the file is held by another process, is on a
    /// network filesystem or is not a state store; the server cannot be reached or does
    /// not flush commits; or a password is given for a SQLite store.
    pub fn open(&self, password: Option<&str>) -> Result<StateStore, StateError> {
        let store = match self {
            Self::Sqlite(path) => {
                if password.is_some() {
                    return Err(open_error("a password is for a postgres:// store"));
                }
                open_sqlite(path)?
            }
            Self::Postgres(url) => open_postgres(url, password)?,
        };
        Ok(StateStore {
            store,
            epoch: self.epoch_policy(),
        })
    }
}

impl crate::Args {
    /// The state store `--state-store` names, opened, or `None` when the flag is not
    /// given (the scheduler's state is then in memory).
    ///
    /// # Errors
    /// [`StateError`]: see [`StateStoreUri::open`]; or the password file cannot be read.
    pub fn open_state_store(&self) -> Result<Option<StateStore>, StateError> {
        let Some(uri) = &self.state_store else {
            return Ok(None);
        };
        let password = self
            .state_store_password_file
            .as_deref()
            .map(read_password)
            .transpose()?;
        uri.open(password.as_deref()).map(Some)
    }
}

fn open_error(why: impl fmt::Display) -> StateError {
    StateError::Open(why.to_string())
}

fn open_sqlite(_path: &Path) -> Result<Arc<dyn Store>, StateError> {
    // Not wired yet: the tests of the flag fail against this.
    Err(StateError::NoBackend("sqlite"))
}

#[cfg(not(feature = "postgres"))]
fn open_postgres(_url: &str, _password: Option<&str>) -> Result<Arc<dyn Store>, StateError> {
    Err(StateError::NoBackend("postgres"))
}

#[cfg(feature = "postgres")]
fn open_postgres(url: &str, password: Option<&str>) -> Result<Arc<dyn Store>, StateError> {
    use kbf_state::postgres::{PostgresConfig, PostgresStore};
    let mut config = PostgresConfig::parse(url).map_err(open_error)?;
    if let Some(password) = password {
        config = config.with_password(password);
    }
    let store = off_runtime(|| PostgresStore::open(&config)).map_err(open_error)?;
    Ok(Arc::new(OffRuntime(store)))
}

/// Reads the password in `--state-store-password-file`: its first line, with no line
/// ending. The text is never put in an error.
///
/// # Errors
/// [`StateError::Open`] when the file cannot be read or is empty.
pub fn read_password(file: &Path) -> Result<String, StateError> {
    let text = std::fs::read_to_string(file).map_err(|e| {
        open_error(format!(
            "--state-store-password-file {}: {e}",
            file.display()
        ))
    })?;
    let password = text.lines().next().unwrap_or_default();
    if password.is_empty() {
        return Err(open_error(format!(
            "--state-store-password-file {} is empty",
            file.display()
        )));
    }
    Ok(password.to_owned())
}

/// Runs `call` on a thread of its own and waits. The Postgres client drives its own
/// runtime and panics when a call is made from a thread that is running another one,
/// which every call from a gRPC handler or a start is.
#[allow(dead_code)]
fn off_runtime<T: Send>(call: impl FnOnce() -> T + Send) -> T {
    // Not off the runtime yet: the test of the wrapper fails against this.
    call()
}

/// A store whose every call runs [`off_runtime`].
#[allow(dead_code)]
struct OffRuntime<S>(S);

#[allow(dead_code)]
impl<S: Store> Store for OffRuntime<S> {
    fn apply_batch(
        &self,
        batch: Vec<kbf_state::Txn>,
    ) -> Result<Vec<Result<kbf_state::Commit, kbf_state::Conflict>>, kbf_state::StoreError> {
        off_runtime(|| self.0.apply_batch(batch))
    }

    fn get(&self, key: &str) -> Result<Option<kbf_state::Versioned>, kbf_state::StoreError> {
        off_runtime(|| self.0.get(key))
    }

    fn scan_prefix(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, kbf_state::Versioned)>, kbf_state::StoreError> {
        off_runtime(|| self.0.scan_prefix(prefix))
    }

    fn changes(
        &self,
        after: kbf_state::Revision,
        limit: usize,
    ) -> Result<kbf_state::Changes, kbf_state::StoreError> {
        off_runtime(|| self.0.changes(after, limit))
    }

    fn reap_expired(&self) -> Result<usize, kbf_state::StoreError> {
        off_runtime(|| self.0.reap_expired())
    }

    fn commit_stats(&self) -> kbf_state::CommitStats {
        off_runtime(|| self.0.commit_stats())
    }

    fn now(&self) -> kbf_state::StoreTime {
        off_runtime(|| self.0.now())
    }
}

/// An open state store, and what a start does with its epoch.
pub struct StateStore {
    /// The store.
    pub store: Arc<dyn Store>,
    /// The epoch the start takes.
    pub epoch: EpochPolicy,
}

#[cfg(any())]
mod tests;
