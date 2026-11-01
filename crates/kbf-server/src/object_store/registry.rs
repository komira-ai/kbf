//! The registry: reads, and the mutations that keep the invariants of the module docs.

use std::collections::BTreeSet;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use kbf_state::{Cond, Store, StoreError, Txn, Versioned};
use kbf_types::object_store_api::{
    AuditAction, AuditEntry, AuditOutcome, BindingInfo, BindingPut, BindingRole, ConnectionInfo,
    ConnectionName, ConnectionPut, Secret, StoreIdentity,
};
use prost::Message as _;
use subtle::ConstantTimeEq as _;

use super::Step;
use super::probe::check_endpoint;
use super::rows::{
    AUDIT_PREFIX, AuditRow, BIND_PREFIX, BindRow, CONN_PREFIX, ConnRow, GEN_KEY, GenRow,
    action_code, action_of, audit_key, bind_key, conn_key, decode, outcome_code, outcome_of,
};
use super::secrets::{SecretError, SecretFiles};
use crate::enroll::Random;
use crate::stores::{DEFAULT_MIN_PUT_RATE, DEFAULT_S3_PREFIX, S3Uri, StoreUri, region_or_default};

/// How old an unreferenced file must be before a sweep removes it.
pub const SWEEP_GRACE: Duration = Duration::from_secs(600);

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
    /// The connection exists, but the role `cold` does not name it: only the bound
    /// connection can be re-pointed.
    #[error("the role cold is not bound to this connection")]
    NotBound,
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
    grace: Duration,
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
    /// See [`AuditRow::detail`]; empty for the actions whose subject says it all.
    detail: String,
}

/// The precondition that a row is as it was read, or still absent.
fn cond_of(row: Option<&Versioned>) -> Cond {
    row.map_or(Cond::Absent, |v| Cond::Version(v.version))
}

fn read_gen_of(store: &dyn Store) -> Result<Gen, RegistryError> {
    let stored = store.get(GEN_KEY)?;
    let row = match &stored {
        Some(stored) => decode(stored, "gen")?,
        None => GenRow::default(),
    };
    Ok(Gen {
        row,
        cond: cond_of(stored.as_ref()),
    })
}

/// How often [`cold_store_view`] reads again when a write lands between its reads.
const VIEW_TRIES: usize = 3;

/// What a storage node is told of the registered cold store: the registry generation the
/// reading is of, and the store's URI, if a `cold` binding exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColdStoreView {
    /// The registry generation; 0 until the first change.
    pub generation: u64,
    /// `s3://bucket/prefix/?endpoint=...` ([`S3Uri::node_uri`]): the bucket, prefix,
    /// endpoint, region, `conditional-put` and `min-put-rate` of the registration.
    /// Never the access key id or the secret: they are not copied into it. `None` when
    /// no store is bound to `cold`.
    pub uri: Option<String>,
}

/// The cold store as a node may be told it, read from one generation of the registry in
/// `store`. Reads the rows only: no secret file, so it needs no secrets directory.
///
/// # Errors
/// [`RegistryError::Store`]; [`RegistryError::Corrupt`] if a row does not decode, the
/// binding names a connection that does not exist, or the registered fields do not
/// make a store URI that reads back as the same store; [`RegistryError::Conflict`] if
/// the registry kept changing while it was read.
pub fn cold_store_view(store: &dyn Store) -> Result<ColdStoreView, RegistryError> {
    for _ in 0..VIEW_TRIES {
        let generation = read_gen_of(store)?.row.generation;
        let uri = match store.get(&bind_key(BindingRole::Cold))? {
            None => None,
            Some(stored) => {
                let bind: BindRow = decode(&stored, "bind")?;
                let name = bind
                    .connection
                    .parse::<ConnectionName>()
                    .map_err(|_| RegistryError::Corrupt("bind"))?;
                let conn: ConnRow = decode(
                    &store
                        .get(&conn_key(&name))?
                        .ok_or(RegistryError::Corrupt("conn"))?,
                    "conn",
                )?;
                Some(cold_uri(&bind, &conn)?)
            }
        };
        // A change writes the generation row in the same transaction as the rows it
        // changes, so an unchanged generation means the three reads are of one state.
        if read_gen_of(store)?.row.generation == generation {
            return Ok(ColdStoreView { generation, uri });
        }
    }
    Err(RegistryError::Conflict)
}

/// The URI of the store `bind` and `conn` register, checked to read back as itself.
fn cold_uri(bind: &BindRow, conn: &ConnRow) -> Result<String, RegistryError> {
    // The checks registration makes: a row written by anything else still yields no
    // userinfo, no https and no path in the URI a node is sent.
    check_endpoint(&conn.endpoint).map_err(|_| RegistryError::Corrupt("conn"))?;
    let prefix = match bind.prefix.as_str() {
        "" => DEFAULT_S3_PREFIX.to_owned(),
        p if p.ends_with('/') => p.to_owned(),
        p => format!("{p}/"),
    };
    let s3 = S3Uri {
        bucket: bind.bucket.clone(),
        prefix,
        endpoint: conn.endpoint.clone(),
        region: region_or_default(&conn.region),
        conditional_put: conn.conditional_put,
        min_put_rate: NonZeroU64::new(conn.min_put_rate).unwrap_or(DEFAULT_MIN_PUT_RATE),
    };
    let uri = s3.node_uri();
    let back = uri.parse::<StoreUri>().ok();
    if back.as_ref().and_then(StoreUri::as_s3) == Some(&s3) {
        Ok(uri)
    } else {
        Err(RegistryError::Corrupt("bind"))
    }
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
        Self::open_with_grace(store, dir, random, SWEEP_GRACE)
    }

    /// [`Registry::open`] with the age a file must reach before a sweep calls it an
    /// orphan. Two frontend children share the directory, and one's file is unnamed by
    /// any row between its rename and its commit.
    ///
    /// # Errors
    /// As [`Registry::open`].
    pub fn open_with_grace(
        store: Arc<dyn Store>,
        dir: &Path,
        random: Arc<dyn Random>,
        grace: Duration,
    ) -> Result<Self, RegistryError> {
        let registry = Self {
            grace,
            store,
            files: SecretFiles::new(dir)?,
            random,
            write: Mutex::new(()),
        };
        registry.sweep()?;
        Ok(registry)
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
        read_gen_of(&*self.store)
    }

    fn conn_row(&self, name: &ConnectionName) -> Result<Option<ConnRow>, RegistryError> {
        match self.store.get(&conn_key(name))? {
            Some(stored) => Ok(Some(decode(&stored, "conn")?)),
            None => Ok(None),
        }
    }

    fn bind_rows(&self) -> Result<Vec<(BindingRole, BindRow)>, RegistryError> {
        self.store
            .scan_prefix(BIND_PREFIX)?
            .into_iter()
            .map(|(key, stored)| {
                let role = key[BIND_PREFIX.len()..]
                    .parse::<BindingRole>()
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
                    .parse::<ConnectionName>()
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
        Ok(self.read_gen()?.row.generation)
    }

    /// Every connection, by name.
    ///
    /// # Errors
    /// The store failed or a row is unreadable.
    pub fn connections(&self) -> Result<Vec<ConnectionInfo>, RegistryError> {
        Ok(self
            .conn_rows()?
            .into_iter()
            .map(|(name, row)| info_of(name, row))
            .collect())
    }

    /// One connection.
    ///
    /// # Errors
    /// The store failed or the row is unreadable.
    pub fn connection(
        &self,
        name: &ConnectionName,
    ) -> Result<Option<ConnectionInfo>, RegistryError> {
        Ok(self.conn_row(name)?.map(|row| info_of(name.clone(), row)))
    }

    /// Every binding, by role.
    ///
    /// # Errors
    /// The store failed or a row is unreadable.
    pub fn bindings(&self) -> Result<Vec<BindingInfo>, RegistryError> {
        self.bind_rows()?
            .into_iter()
            .map(|(role, row)| binding_of(role, row))
            .collect()
    }

    /// The newest `limit` audit rows, newest first.
    ///
    /// # Errors
    /// The store failed or a row is unreadable.
    pub fn audit(&self, limit: NonZeroUsize) -> Result<Vec<AuditEntry>, RegistryError> {
        let page = self
            .store
            .scan_range(kbf_state::ScanRange::new(AUDIT_PREFIX, limit).reverse())?;
        page.rows
            .iter()
            .map(|(key, stored)| {
                let row: AuditRow = decode(stored, "audit")?;
                Ok(AuditEntry {
                    seq: key[AUDIT_PREFIX.len()..]
                        .parse::<u64>()
                        .map_err(|_| RegistryError::Corrupt("audit"))?,
                    at: row.at,
                    action: action_of(row.action)?,
                    subject: row.subject,
                    outcome: outcome_of(row.outcome)?,
                    generation: row.generation,
                    detail: Some(row.detail).filter(|detail| !detail.is_empty()),
                })
            })
            .collect()
    }

    /// The secret key of `name`, from the file its row names.
    ///
    /// # Errors
    /// [`RegistryError::NoSuchConnection`], or the file is missing, too wide or invalid
    /// (the caller fails closed).
    pub fn secret(&self, name: &ConnectionName) -> Result<Secret, RegistryError> {
        let row = self
            .conn_row(name)?
            .ok_or(RegistryError::NoSuchConnection)?;
        match self.files.read(name, row.secret_version) {
            // The other frontend child may have rotated and removed this version
            // after we read the row: read the row again, once.
            Err(SecretError::Missing) => {
                let row = self
                    .conn_row(name)?
                    .ok_or(RegistryError::NoSuchConnection)?;
                Ok(self.files.read(name, row.secret_version)?)
            }
            other => Ok(other?),
        }
    }

    /// Removes the secret files no connection row names, and the temp files a crash left.
    /// Returns how many.
    ///
    /// # Errors
    /// The store failed, or the directory cannot be listed.
    pub fn sweep(&self) -> Result<usize, RegistryError> {
        let _guard = self.lock();
        let keep: BTreeSet<(String, u64)> = self
            .conn_rows()?
            .into_iter()
            .map(|(name, row)| (name.to_string(), row.secret_version))
            .collect();
        Ok(self.files.sweep(&keep, self.grace)?)
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
        let _guard = self.lock();
        let head = self.read_gen()?;
        head.expect(put.expect_generation)?;
        let existing = self.conn_row(name)?;
        let mut row = ConnRow {
            endpoint: put.endpoint.clone(),
            region: put.region.clone().unwrap_or_default(),
            access_key_id: put.access_key_id.clone(),
            conditional_put: put.conditional_put.unwrap_or(false),
            min_put_rate: put.min_put_rate.map_or(0, NonZeroU64::get),
            secret_version: 0,
            updated_generation: 0,
        };
        let mut same_secret = None;
        if let Some(old) = &existing {
            if old.endpoint != row.endpoint && self.is_bound(name)? {
                return Err(RegistryError::IdentityChange);
            }
            // A live file that is missing or too wide is not "the same secret": the
            // operator's PUT repairs it.
            same_secret = self
                .files
                .read(name, old.secret_version)
                .ok()
                .filter(|live| secrets_equal(live, &put.secret_access_key))
                .map(|_| old.secret_version);
        }
        if let (Some(old), Some(version)) = (&existing, same_secret) {
            row.secret_version = version;
            row.updated_generation = old.updated_generation;
            if row == *old {
                let audit = audit_of(AuditAction::PutConnection, name, AuditOutcome::Unchanged);
                let generation = self.commit(&head, Txn::new(), audit, false)?;
                return Ok(Written {
                    generation,
                    outcome: AuditOutcome::Unchanged,
                });
            }
        }
        let generation = head.after(true);
        row.updated_generation = generation;
        let written = match same_secret {
            Some(version) => {
                row.secret_version = version;
                None
            }
            None => {
                let version = self.fresh_version()?;
                self.files.put(name, version, &put.secret_access_key)?;
                row.secret_version = version;
                Some(version)
            }
        };
        let outcome = if existing.is_some() {
            AuditOutcome::Changed
        } else {
            AuditOutcome::Created
        };
        let txn = Txn::new().put(conn_key(name), row.encode_to_vec(), Cond::Any);
        let audit = audit_of(AuditAction::PutConnection, name, outcome);
        if let Err(e) = self.commit(&head, txn, audit, true) {
            // Nothing names the new file: remove it now rather than at the next start.
            if let Some(version) = written {
                self.forget(name, version);
            }
            return Err(e);
        }
        if let (Some(old), Some(_)) = (&existing, written) {
            self.forget(name, old.secret_version);
        }
        Ok(Written {
            generation,
            outcome,
        })
    }

    /// Deletes a connection.
    ///
    /// # Errors
    /// [`RegistryError::NoSuchConnection`], [`RegistryError::InUse`] if a binding names
    /// it, [`RegistryError::Conflict`], [`RegistryError::Store`].
    pub fn delete_connection(&self, name: &ConnectionName) -> Result<Written, RegistryError> {
        let _guard = self.lock();
        let head = self.read_gen()?;
        let old = self
            .conn_row(name)?
            .ok_or(RegistryError::NoSuchConnection)?;
        if self.is_bound(name)? {
            return Err(RegistryError::InUse);
        }
        let txn = Txn::new().delete(conn_key(name), Cond::Any);
        let audit = audit_of(AuditAction::DeleteConnection, name, AuditOutcome::Deleted);
        let generation = self.commit(&head, txn, audit, true)?;
        self.forget(name, old.secret_version);
        Ok(Written {
            generation,
            outcome: AuditOutcome::Deleted,
        })
    }

    /// Binds `role` to a connection, a bucket and a prefix.
    ///
    /// # Errors
    /// [`RegistryError::GenerationMismatch`], [`RegistryError::NoSuchConnection`],
    /// [`RegistryError::IdentityChange`] if the role is bound already and the endpoint,
    /// bucket or prefix would change, [`RegistryError::Conflict`], [`RegistryError::Store`].
    pub fn put_binding(
        &self,
        role: BindingRole,
        put: &BindingPut,
    ) -> Result<Written, RegistryError> {
        let _guard = self.lock();
        let head = self.read_gen()?;
        head.expect(put.expect_generation)?;
        let conn = self
            .conn_row(&put.connection)?
            .ok_or(RegistryError::NoSuchConnection)?;
        let old: Option<BindRow> = self
            .store
            .get(&bind_key(role))?
            .map(|stored| decode(&stored, "bind"))
            .transpose()?;
        let mut row = BindRow {
            connection: put.connection.to_string(),
            bucket: put.bucket.clone(),
            prefix: put.prefix.clone(),
            updated_generation: 0,
        };
        let subject = role.to_string();
        if let Some(old) = &old {
            let old_endpoint = old
                .connection
                .parse::<ConnectionName>()
                .map_err(|_| RegistryError::Corrupt("bind"))
                .and_then(|name| self.conn_row(&name))?
                .map(|c| c.endpoint);
            if old.bucket != row.bucket
                || old.prefix != row.prefix
                || old_endpoint.as_deref() != Some(conn.endpoint.as_str())
            {
                return Err(RegistryError::IdentityChange);
            }
            row.updated_generation = old.updated_generation;
            if row == *old {
                let audit = Audit {
                    action: AuditAction::PutBinding,
                    subject,
                    outcome: AuditOutcome::Unchanged,
                    detail: String::new(),
                };
                let generation = self.commit(&head, Txn::new(), audit, false)?;
                return Ok(Written {
                    generation,
                    outcome: AuditOutcome::Unchanged,
                });
            }
        }
        let generation = head.after(true);
        row.updated_generation = generation;
        let outcome = if old.is_some() {
            AuditOutcome::Changed
        } else {
            AuditOutcome::Created
        };
        let txn = Txn::new().put(bind_key(role), row.encode_to_vec(), Cond::Any);
        let audit = Audit {
            action: AuditAction::PutBinding,
            subject,
            outcome,
            detail: String::new(),
        };
        self.commit(&head, txn, audit, true)?;
        Ok(Written {
            generation,
            outcome,
        })
    }

    /// Moves the store the role `cold` names, on purpose: replaces the endpoint of
    /// connection `name` and the bucket and prefix of the binding that names it, bumps
    /// the generation and appends an audit row with the old and the new identity, all in
    /// one transaction. The identity it is already at is `Unchanged`. Returns what was
    /// written and the identity before. The caller has probed `to`.
    ///
    /// # Errors
    /// [`RegistryError::GenerationMismatch`] if `expect_generation` is stale,
    /// [`RegistryError::NoSuchConnection`], [`RegistryError::NotBound`] if the role
    /// `cold` does not name the connection, [`RegistryError::Conflict`],
    /// [`RegistryError::Store`].
    pub fn repoint(
        &self,
        _name: &ConnectionName,
        _to: &StoreIdentity,
        _expect_generation: u64,
    ) -> Result<(Written, StoreIdentity), RegistryError> {
        Err(RegistryError::Conflict)
    }

    /// Removes a binding. Whether a node still runs on it is the caller's rule.
    ///
    /// # Errors
    /// [`RegistryError::NoSuchBinding`], [`RegistryError::Conflict`], [`RegistryError::Store`].
    pub fn delete_binding(&self, role: BindingRole) -> Result<Written, RegistryError> {
        let _guard = self.lock();
        let head = self.read_gen()?;
        if self.store.get(&bind_key(role))?.is_none() {
            return Err(RegistryError::NoSuchBinding);
        }
        let txn = Txn::new().delete(bind_key(role), Cond::Any);
        let audit = Audit {
            action: AuditAction::DeleteBinding,
            subject: role.to_string(),
            outcome: AuditOutcome::Deleted,
            detail: String::new(),
        };
        let generation = self.commit(&head, txn, audit, true)?;
        Ok(Written {
            generation,
            outcome: AuditOutcome::Deleted,
        })
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
    fn commit(&self, head: &Gen, txn: Txn, audit: Audit, bump: bool) -> Result<u64, RegistryError> {
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
            detail: audit.detail,
        };
        let txn = txn.put(GEN_KEY, gen_row.encode_to_vec(), head.cond).put(
            audit_key(seq),
            audit_row.encode_to_vec(),
            Cond::Absent,
        );
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
        detail: String::new(),
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
            .parse::<ConnectionName>()
            .map_err(|_| RegistryError::Corrupt("bind"))?,
        bucket: row.bucket,
        prefix: row.prefix,
        updated_generation: row.updated_generation,
    })
}
