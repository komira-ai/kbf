#![allow(dead_code, unused_imports, unused_variables)]
//! STUB (red-first). The registry: reads, and the mutations that keep the invariants of the module docs.

use std::collections::BTreeSet;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use kbf_state::{Cond, Store, StoreError, Txn, Versioned};
use kbf_types::object_store_api::{
    AuditAction, AuditEntry, AuditOutcome, BindingInfo, BindingPut, BindingRole, ConnectionInfo,
    ConnectionName, ConnectionPut, Secret,
};
use prost::Message as _;
use subtle::ConstantTimeEq as _;

use super::rows::{
    AUDIT_PREFIX, AuditRow, BIND_PREFIX, BindRow, CONN_PREFIX, ConnRow, GEN_KEY, GenRow,
    action_code, action_of, audit_key, bind_key, conn_key, decode, outcome_code, outcome_of,
};
use super::secrets::{SecretError, SecretFiles};
use super::Step;
use crate::enroll::Random;

/// Why a registry call did not do what was asked. Nothing here carries a secret.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// `expect_generation` named a generation the registry has left.
    #[error("the registry is at generation {current}")]
    GenerationMismatch {
        /// The generation now.
        current: u64,
    },
    /// Another writer changed the registry while this call ran; nothing was written.
    #[error("the registry changed during the call; retry")]
    Conflict,
    /// No such connection (or the binding names one that does not exist).
    #[error("no such connection")]
    NoSuchConnection,
    /// No such binding.
    #[error("no such binding")]
    NoSuchBinding,
    /// A binding still uses the connection.
    #[error("a binding uses this connection")]
    InUse,
    /// The change would move a bound store: its endpoint, bucket or prefix. Credentials
    /// may change; the store may not.
    #[error("the store identity (endpoint, bucket, prefix) of a bound role cannot change")]
    IdentityChange,
    /// A secret file could not be written, or the live one is missing or too wide.
    #[error(transparent)]
    Secret(#[from] SecretError),
    /// A stored row does not decode; the text names its family.
    #[error("the {0} rows are not readable")]
    Corrupt(&'static str),
    /// The state store failed, or this frontend is no longer its writer
    /// ([`StoreError::Fenced`]).
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// What a mutation did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Written {
    /// The registry generation after the call.
    pub generation: u64,
    /// `Unchanged` means nothing but an audit row was written.
    pub outcome: AuditOutcome,
}

/// The registration of the object store: see the module docs.
pub struct Registry {
    store: Arc<dyn Store>,
    files: SecretFiles,
    random: Arc<dyn Random>,
    /// Serializes this process's mutations and sweeps, so a sweep never removes the file
    /// of a mutation that has not committed its row yet.
    write: Mutex<()>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("files", &self.files)
            .finish_non_exhaustive()
    }
}

/// The generation row as read, and the precondition that writes it back.
struct Gen {
    row: GenRow,
    cond: Cond,
}

impl Gen {
    fn expect(&self, expected: Option<u64>) -> Result<(), RegistryError> {
        match expected {
            Some(generation) if generation != self.row.generation => {
                Err(RegistryError::GenerationMismatch {
                    current: self.row.generation,
                })
            }
            _ => Ok(()),
        }
    }

    fn after(&self, bump: bool) -> u64 {
        self.row.generation + u64::from(bump)
    }
}

/// What the audit row of a mutation says.
struct Audit {
    action: AuditAction,
    subject: String,
    outcome: AuditOutcome,
}

/// The precondition that a row is as it was read, or still absent.
fn cond_of(row: Option<&Versioned>) -> Cond {
    row.map_or(Cond::Absent, |v| Cond::Version(v.version))
}

impl Registry {
    /// The registry over `store` (the frontend's fenced handle, so every write checks
    /// that this frontend still holds the writer role), with secret files in `dir`.
    /// Sweeps the files no row names: temp files and versions a crash left.
    ///
    /// # Errors
    /// The directory cannot be made or listed, or a row cannot be read.
    pub fn open(
        store: Arc<dyn Store>,
        dir: &Path,
        random: Arc<dyn Random>,
    ) -> Result<Self, RegistryError> {
        let _ = (&dir, &random);
        Ok(Self { store, files: SecretFiles::new(dir)?, random, write: Mutex::new(()) })
    }

    /// Runs `hook` at each [`Step`] of the write order, before it happens (tests only).
    #[cfg(test)]
    pub(super) fn set_failpoint(&self, hook: impl Fn(Step) + Send + Sync + 'static) {
        self.files.set_failpoint(hook);
    }

    fn lock(&self) -> MutexGuard<'_, ()> {
        self.write.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn read_gen(&self) -> Result<Gen, RegistryError> {
        let stored = self.store.get(GEN_KEY)?;
        let row = match &stored {
            Some(stored) => decode(stored, "gen")?,
            None => GenRow::default(),
        };
        Ok(Gen {
            row,
            cond: cond_of(stored.as_ref()),
        })
    }

    fn conn_row(&self, name: &ConnectionName) -> Result<Option<(ConnRow, Versioned)>, RegistryError> {
        match self.store.get(&conn_key(name))? {
            Some(stored) => Ok(Some((decode(&stored, "conn")?, stored))),
            None => Ok(None),
        }
    }

    fn bind_rows(&self) -> Result<Vec<(BindingRole, BindRow)>, RegistryError> {
        self.store
            .scan_prefix(BIND_PREFIX)?
            .into_iter()
            .map(|(key, stored)| {
                let role = key[BIND_PREFIX.len()..]
                    .parse()
                    .map_err(|_| RegistryError::Corrupt("bind"))?;
                Ok((role, decode(&stored, "bind")?))
            })
            .collect()
    }

    fn conn_rows(&self) -> Result<Vec<(ConnectionName, ConnRow)>, RegistryError> {
        self.store
            .scan_prefix(CONN_PREFIX)?
            .into_iter()
            .map(|(key, stored)| {
                let name = key[CONN_PREFIX.len()..]
                    .parse()
                    .map_err(|_| RegistryError::Corrupt("conn"))?;
                Ok((name, decode(&stored, "conn")?))
            })
            .collect()
    }

    /// The registry generation: 0 until the first change.
    ///
    /// # Errors
    /// The store failed or the row is unreadable.
    pub fn generation(&self) -> Result<u64, RegistryError> {
        Ok(0)
    }

    /// Every connection, by name.
    ///
    /// # Errors
    /// The store failed or a row is unreadable.
    pub fn connections(&self) -> Result<Vec<ConnectionInfo>, RegistryError> {
        Ok(Vec::new())
    }

    /// One connection.
    ///
    /// # Errors
    /// The store failed or the row is unreadable.
    pub fn connection(&self, name: &ConnectionName) -> Result<Option<ConnectionInfo>, RegistryError> {
        let _ = name;
        Ok(None)
    }

    /// Every binding, by role.
    ///
    /// # Errors
    /// The store failed or a row is unreadable.
    pub fn bindings(&self) -> Result<Vec<BindingInfo>, RegistryError> {
        Ok(Vec::new())
    }

    /// The newest `limit` audit rows, newest first.
    ///
    /// # Errors
    /// The store failed or a row is unreadable.
    pub fn audit(&self, limit: NonZeroUsize) -> Result<Vec<AuditEntry>, RegistryError> {
        let _ = limit;
        Ok(Vec::new())
    }

    /// The secret key of `name`, from the file its row names.
    ///
    /// # Errors
    /// [`RegistryError::NoSuchConnection`], or the file is missing, too wide or invalid
    /// (the caller fails closed).
    pub fn secret(&self, name: &ConnectionName) -> Result<Secret, RegistryError> {
        let _ = name;
        Err(RegistryError::NoSuchConnection)
    }

    /// Removes the secret files no connection row names, and the temp files a crash left.
    /// Returns how many.
    ///
    /// # Errors
    /// The store failed, or the directory cannot be listed.
    pub fn sweep(&self) -> Result<usize, RegistryError> {
        Ok(0)
    }

    /// Creates or replaces a connection.
    ///
    /// The same content again (every field and the secret) writes only an audit row and
    /// leaves the generation. A new secret is written as a new versioned file first and
    /// named by the row only once it is durable; the old version is removed after the row
    /// commits.
    ///
    /// # Errors
    /// [`RegistryError::GenerationMismatch`] if `put.expect_generation` is stale,
    /// [`RegistryError::IdentityChange`] if the endpoint of a bound connection would
    /// change, [`RegistryError::Conflict`] if another writer got in, [`RegistryError::Secret`]
    /// and [`RegistryError::Store`] for a failing file system or store.
    pub fn put_connection(
        &self,
        name: &ConnectionName,
        put: &ConnectionPut,
    ) -> Result<Written, RegistryError> {
        let _ = (name, put);
        Err(RegistryError::Conflict)
    }

    /// Deletes a connection.
    ///
    /// # Errors
    /// [`RegistryError::NoSuchConnection`], [`RegistryError::InUse`] if a binding names
    /// it, [`RegistryError::Conflict`], [`RegistryError::Store`].
    pub fn delete_connection(&self, name: &ConnectionName) -> Result<Written, RegistryError> {
        let _ = name;
        Err(RegistryError::Conflict)
    }

    /// Binds `role` to a connection, a bucket and a prefix.
    ///
    /// # Errors
    /// [`RegistryError::GenerationMismatch`], [`RegistryError::NoSuchConnection`],
    /// [`RegistryError::IdentityChange`] if the role is bound already and the endpoint,
    /// bucket or prefix would change, [`RegistryError::Conflict`], [`RegistryError::Store`].
    pub fn put_binding(&self, role: BindingRole, put: &BindingPut) -> Result<Written, RegistryError> {
        let _ = (role, put);
        Err(RegistryError::Conflict)
    }

    /// Removes a binding. Whether a node still runs on it is the caller's rule.
    ///
    /// # Errors
    /// [`RegistryError::NoSuchBinding`], [`RegistryError::Conflict`], [`RegistryError::Store`].
    pub fn delete_binding(&self, role: BindingRole) -> Result<Written, RegistryError> {
        let _ = role;
        Err(RegistryError::Conflict)
    }

    fn is_bound(&self, name: &ConnectionName) -> Result<bool, RegistryError> {
        Ok(self
            .bind_rows()?
            .iter()
            .any(|(_, row)| row.connection == name.as_str()))
    }

    /// A version no earlier write used: 64 random bits.
    fn fresh_version(&self) -> Result<u64, SecretError> {
        let mut bytes = [0u8; 8];
        self.random
            .fill(&mut bytes)
            .map_err(|source| SecretError::Io {
                op: "draw a version",
                source,
            })?;
        Ok(u64::from_be_bytes(bytes))
    }

    /// Removes a file no row names. A failure leaves an orphan for the next start's sweep.
    fn forget(&self, name: &ConnectionName, version: u64) {
        if let Err(error) = self.files.remove(name, version) {
            tracing::warn!(connection = %name, %error, "an unreferenced secret file stays until the next sweep");
        }
    }

    /// Commits `txn` together with the generation row (compare-and-set against the value
    /// read) and the audit row, in one store transaction. Returns the generation after.
    fn commit(
        &self,
        head: &Gen,
        txn: Txn,
        audit: Audit,
        bump: bool,
    ) -> Result<u64, RegistryError> {
        let generation = head.after(bump);
        let seq = head.row.audit_seq + 1;
        let gen_row = GenRow {
            generation,
            audit_seq: seq,
        };
        let audit_row = AuditRow {
            at: self.store.now().as_duration().as_secs(),
            action: action_code(audit.action),
            subject: audit.subject,
            outcome: outcome_code(audit.outcome),
            generation,
        };
        let txn = txn
            .put(GEN_KEY, gen_row.encode_to_vec(), head.cond)
            .put(audit_key(seq), audit_row.encode_to_vec(), Cond::Absent);
        self.files.reach(Step::CommitRow);
        match self.store.txn(txn)? {
            Ok(_) => Ok(generation),
            Err(_) => Err(RegistryError::Conflict),
        }
    }
}

fn audit_of(action: AuditAction, name: &ConnectionName, outcome: AuditOutcome) -> Audit {
    Audit {
        action,
        subject: name.to_string(),
        outcome,
    }
}

/// Whether two secrets are the same, without a timing difference in where they differ.
fn secrets_equal(a: &Secret, b: &Secret) -> bool {
    bool::from(a.expose().as_bytes().ct_eq(b.expose().as_bytes()))
}

fn info_of(name: ConnectionName, row: ConnRow) -> ConnectionInfo {
    ConnectionInfo {
        name,
        endpoint: row.endpoint,
        region: row.region,
        access_key_id: row.access_key_id,
        conditional_put: row.conditional_put,
        min_put_rate: NonZeroU64::new(row.min_put_rate),
        updated_generation: row.updated_generation,
    }
}

fn binding_of(role: BindingRole, row: BindRow) -> Result<BindingInfo, RegistryError> {
    Ok(BindingInfo {
        role,
        connection: row
            .connection
            .parse()
            .map_err(|_| RegistryError::Corrupt("bind"))?,
        bucket: row.bucket,
        prefix: row.prefix,
        updated_generation: row.updated_generation,
    })
}
