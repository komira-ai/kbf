//! `--state-store`: where the scheduler keeps its state (`docs/design/architecture-v2.md`,
//! section 4.3).

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use kbf_state::Store;

use crate::farm::{EpochPolicy, StateError};
use crate::members::MembershipConfig;

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
            } else {
                Ok(Self::Postgres(s.to_owned()))
            };
        }
        // Only the scheme is echoed: a mistyped postgres value may carry a password.
        let scheme = s.split_once("://").map_or(s, |(scheme, _)| scheme);
        Err(StateStoreUriError::Scheme(scheme.to_owned()))
    }
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

    /// Opens the store.
    ///
    /// # Errors
    /// [`StateError::NoBackend`]: neither backend is part of this build yet.
    pub fn open(&self) -> Result<StateStore, StateError> {
        Err(StateError::NoBackend(self.scheme()))
    }
}

/// An open state store, and what a start does with its epoch.
pub struct StateStore {
    /// The store.
    pub store: Arc<dyn Store>,
    /// The epoch the start takes.
    pub epoch: EpochPolicy,
    /// How long storage members live and dwell in the store ([`crate::members`]).
    pub members: MembershipConfig,
}

#[cfg(test)]
mod tests;
