//! The Postgres backend (feature `postgres`): section 4.3 of `docs/design/architecture-v2.md`,
//! clause by clause.
//!
//! - **C1, C2.** A batch is one `SERIALIZABLE` transaction. Each transaction in it loads
//!   its keys `FOR UPDATE`, judges every precondition against what it loaded, then writes.
//!   A serialization failure runs the batch again.
//! - **C3.** A counter is a row incremented in the writing transaction.
//! - **C4.** Expiry is `expires_us` against the database's `clock_timestamp()`, read
//!   after the head lock and again by every read; a reaper (`reap_expired`) writes the
//!   `Expire` rows. A test injects a clock ([`PostgresStore::with_clock`]) and the same
//!   statements take its reading instead.
//! - **C5.** Revisions come from one head row that a writer locks first and holds to its
//!   commit, so they commit in the order they were taken. A writer queues on a table lock
//!   (`SHARE UPDATE EXCLUSIVE`) before its snapshot, so waiting costs no
//!   serialization retry. [`Store::changes`] reads the
//!   committed head, then the rows up to it, and returns the head as its cursor: nothing
//!   at or below it can still commit. A bare sequence would not do (a later number can
//!   commit first and a poller steps over the earlier one); [`Flaw::SequenceRevisions`]
//!   is that mistake, kept so the suite can show it fails. Polling is the baseline;
//!   this module sends no `NOTIFY`.
//! - **C6.** `synchronous_commit` must be on: a connection where it is not is refused,
//!   so an acknowledged write has been flushed.
//! - **C7.** A batch is one transaction and one commit, however many transactions it
//!   holds; the server's WAL flush groups commits from concurrent callers.
//!
//! [`PostgresStore::claim`] is the claim path: read committed, rows locked
//! `FOR UPDATE SKIP LOCKED`, each claimed through the same version compare-and-set.
//!
//! Limits: the feed table grows until a retention pass trims it (not written yet); the
//! client does not speak TLS, so an `sslmode=require` URL is refused instead of
//! downgraded; calls block, so an async caller uses a blocking task.

#[cfg(any(test, feature = "test-support"))]
mod backend;
mod conn;
pub mod schema;
mod session;
mod tx;

use std::fmt;
use std::ops::Bound;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use postgres::NoTls;
use postgres::config::SslMode;
use postgres::error::SqlState;
use postgres::types::ToSql;

#[cfg(any(test, feature = "test-support"))]
pub use backend::PostgresBackend;
use session::{Edge, Pool, RangeShape, Session};
use tx::{Level, Row, Tx, micros};

use crate::clock::{Clock, StoreTime};
use crate::store::{Announcement, LivenessGuard, Span, Store, page_of};
use crate::{
    Change, Changes, Commit, CommitStats, Cond, Conflict, Direction, Event, Revision, ScanPage,
    ScanRange, StoreError, Txn, Version, Versioned,
};

/// The most times a batch is run again after a serialization failure.
const MAX_ATTEMPTS: usize = 200;

/// How many times [`Store::hold_liveness`] tries the lock, and the pause between tries. A
/// [`Store::holder_alive`] probe holds the lock for one statement, so a refusal that
/// outlasts these (about a second) is a real holder.
const HOLD_ATTEMPTS: u32 = 40;
const HOLD_BACKOFF: Duration = Duration::from_millis(25);

/// The dedicated connection that holds a liveness lock: the lock lives as long as the
/// session does.
struct Holder(Mutex<postgres::Client>);

impl Announcement for Holder {
    fn is_held(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .query_one("SELECT 1", &[])
            .is_ok()
    }
}

/// A deliberate defect, to show the suite can fail on a Postgres store.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Flaw {
    /// A correct store.
    #[default]
    None,
    /// C5: revisions come from a sequence, outside the transaction, and the feed
    /// cursor is the last row seen, so a later revision that commits first makes the
    /// poller step over the earlier one.
    SequenceRevisions,
    /// The claim waits for a row another transaction holds instead of skipping it.
    ClaimBlocks,
}

/// Why a connection string is not usable.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// No URL was given.
    #[error("the postgres state store needs a URL (postgres://...)")]
    Missing,
    /// The URL is not a `postgres://` or `postgresql://` URL.
    #[error("the state store URL must start with postgres:// or postgresql://")]
    NotPostgres,
    /// The URL names no host.
    #[error("the state store URL names no host")]
    NoHost,
    /// The URL asks for TLS, which this client does not speak.
    #[error("the state store URL asks for TLS (sslmode), which this client does not support")]
    TlsUnsupported,
    /// The URL does not parse.
    #[error("the state store URL is not valid: {0}")]
    Invalid(String),
    /// The schema name is not lower-case letters, digits and underscores.
    #[error(
        "the state store schema {0:?} must be 1 to 63 characters of a-z, 0-9 and _, not starting with a digit"
    )]
    BadSchema(String),
}

/// Where the store is and how it connects.
#[derive(Clone)]
pub struct PostgresConfig {
    pg: postgres::Config,
    schema: String,
    max_connections: usize,
    supported: schema::Supported,
}

impl fmt::Debug for PostgresConfig {
    // The connection string carries a password; leave it out.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PostgresConfig")
            .field("schema", &self.schema)
            .field("max_connections", &self.max_connections)
            .finish_non_exhaustive()
    }
}

impl PostgresConfig {
    /// The schema a store lives in unless told otherwise.
    pub const DEFAULT_SCHEMA: &'static str = "kbf_state";

    /// The most connections a store opens unless told otherwise. Writers queue on the
    /// commit head, so more would only wait.
    pub const DEFAULT_MAX_CONNECTIONS: usize = 8;

    /// Reads a `postgres://` URL.
    ///
    /// # Errors
    /// [`ConfigError`] when the URL is missing, is not a Postgres URL, names no host or
    /// asks for TLS.
    pub fn parse(url: &str) -> Result<PostgresConfig, ConfigError> {
        let url = url.trim();
        if url.is_empty() {
            return Err(ConfigError::Missing);
        }
        if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
            return Err(ConfigError::NotPostgres);
        }
        let mut pg: postgres::Config = url
            .parse()
            .map_err(|err: postgres::Error| ConfigError::Invalid(err.to_string()))?;
        if pg.get_hosts().is_empty() {
            return Err(ConfigError::NoHost);
        }
        if !matches!(pg.get_ssl_mode(), SslMode::Disable | SslMode::Prefer) {
            return Err(ConfigError::TlsUnsupported);
        }
        if pg.get_application_name().is_none() {
            pg.application_name("kbf-state");
        }
        Ok(PostgresConfig {
            pg,
            schema: Self::DEFAULT_SCHEMA.to_owned(),
            max_connections: Self::DEFAULT_MAX_CONNECTIONS,
            supported: schema::Supported::this_build(),
        })
    }

    /// Keeps the store's tables in `schema`.
    ///
    /// # Errors
    /// [`ConfigError::BadSchema`] when the name is not plain lower case.
    pub fn with_schema(mut self, schema: &str) -> Result<PostgresConfig, ConfigError> {
        let plain = !schema.is_empty()
            && schema.len() <= 63
            && !schema.starts_with(|c: char| c.is_ascii_digit())
            && schema
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !plain {
            return Err(ConfigError::BadSchema(schema.to_owned()));
        }
        self.schema = schema.to_owned();
        Ok(self)
    }

    /// Makes the store accept schema versions `min` to `max` instead of what this build
    /// embeds. For tests that play another release.
    #[doc(hidden)]
    #[must_use]
    pub fn with_supported_schema(mut self, min: u32, max: u32) -> PostgresConfig {
        self.supported = schema::Supported { min, max };
        self
    }

    /// Names the connections in `pg_stat_activity`.
    #[must_use]
    pub fn with_application_name(mut self, name: &str) -> PostgresConfig {
        self.pg.application_name(name);
        self
    }

    /// Opens at most `n` connections (at least one).
    #[must_use]
    pub fn with_max_connections(mut self, n: usize) -> PostgresConfig {
        self.max_connections = n.max(1);
        self
    }

    /// Whether the URL carries a password (user information or a `password` parameter).
    #[must_use]
    pub fn has_password(&self) -> bool {
        self.pg.get_password().is_some()
    }

    /// The schema the store's tables are in.
    #[must_use]
    pub fn schema(&self) -> &str {
        &self.schema
    }
}

/// A failure inside the store, before it is reported as a [`StoreError`].
#[derive(Debug)]
pub(crate) enum Fail {
    Pg(postgres::Error),
    Refused(String),
}

impl From<postgres::Error> for Fail {
    fn from(err: postgres::Error) -> Fail {
        Fail::Pg(err)
    }
}

impl Fail {
    /// A transaction the server rolled back for a reason that running it again cures.
    fn retryable(&self) -> bool {
        match self {
            Fail::Pg(err) => err.code().is_some_and(|code| {
                *code == SqlState::T_R_SERIALIZATION_FAILURE
                    || *code == SqlState::T_R_DEADLOCK_DETECTED
            }),
            Fail::Refused(_) => false,
        }
    }

    fn into_store(self) -> StoreError {
        StoreError::Failed(match self {
            Fail::Pg(err) => describe(&err),
            Fail::Refused(why) => why,
        })
    }
}

/// A row claimed by [`PostgresStore::claim`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claimed {
    /// The key.
    pub key: String,
    /// The value as it was when claimed.
    pub was: Versioned,
    /// The commit of the transaction the caller built for it.
    pub commit: Commit,
}

/// A failed client call, as the store reports it.
fn failed(err: postgres::Error) -> StoreError {
    Fail::Pg(err).into_store()
}

/// What the server said, which `Display` for the client's error leaves out ("db error").
fn describe(err: &postgres::Error) -> String {
    if let Some(db) = err.as_db_error() {
        return format!(
            "{}: {} (SQLSTATE {})",
            db.severity(),
            db.message(),
            db.code().code()
        );
    }
    match std::error::Error::source(err) {
        Some(source) => format!("{err}: {source}"),
        None => err.to_string(),
    }
}

fn to_time(us: i64) -> StoreTime {
    StoreTime::ZERO.saturating_add(Duration::from_micros(u64::try_from(us).unwrap_or(0)))
}

fn from_time(time: StoreTime) -> i64 {
    micros(time.since(StoreTime::ZERO))
}

fn versioned(row: &Row) -> Versioned {
    Versioned {
        value: row.value.clone(),
        version: Version(u64::try_from(row.version).unwrap_or(0)),
        expires_at: row.expires_us.map(to_time),
    }
}

/// The state store on Postgres.
pub struct PostgresStore {
    pool: Pool,
    config: PostgresConfig,
    clock: Option<Arc<dyn Clock>>,
    flaw: Flaw,
    txns: AtomicU64,
    syncs: AtomicU64,
    retries: AtomicU64,
}

/// A batch run in a database transaction that is not committed yet. Test support: it is
/// how a test holds one commit back while another goes through.
#[doc(hidden)]
pub struct Pending<'s> {
    store: &'s PostgresStore,
    tx: Tx<'s>,
    results: Vec<Result<Commit, Conflict>>,
}

impl Pending<'_> {
    /// Commits, and returns the result of each transaction.
    ///
    /// # Errors
    /// [`StoreError`] when the commit fails.
    pub fn commit(self) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        let Pending { store, tx, results } = self;
        let wrote = tx.dirty();
        tx.commit().map_err(Fail::into_store)?;
        store.count(&results, wrote);
        Ok(results)
    }
}

impl PostgresStore {
    /// Connects, checks that the server makes commits durable and that the schema is one
    /// this build supports ([`schema`]). It creates nothing: `kbf-installer` owns the
    /// schema. Expiry is judged by the database's clock.
    ///
    /// # Errors
    /// [`StoreError`] when the server cannot be reached, does not flush commits
    /// (`synchronous_commit` is not on), or holds no schema or one outside the supported
    /// range (one line saying which and what to run).
    pub fn open(config: &PostgresConfig) -> Result<PostgresStore, StoreError> {
        let pool = Pool::open(
            config.pg.clone(),
            config.schema.clone(),
            config.max_connections,
            config.supported,
        )
        .map_err(Fail::into_store)?;
        Ok(PostgresStore {
            pool,
            config: config.clone(),
            clock: None,
            flaw: Flaw::None,
            txns: AtomicU64::new(0),
            syncs: AtomicU64::new(0),
            retries: AtomicU64::new(0),
        })
    }

    /// Judges expiry by `clock` instead of the database's. For tests.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> PostgresStore {
        self.clock = Some(clock);
        self
    }

    /// Plants a deliberate defect. For tests.
    #[must_use]
    pub fn with_flaw(mut self, flaw: Flaw) -> PostgresStore {
        self.flaw = flaw;
        self
    }

    /// How many times a batch has been run again because the server rolled it back for a
    /// serialization failure or a deadlock. A writer that waits its turn for the commit
    /// head costs none.
    #[must_use]
    pub fn retries(&self) -> u64 {
        self.retries.load(Ordering::Relaxed)
    }

    /// The configuration this store was opened with.
    #[must_use]
    pub fn config(&self) -> &PostgresConfig {
        &self.config
    }

    /// Opens the same database again with the same clock and flaw, dropping this
    /// store's connections: a crash of the frontend, for a test.
    ///
    /// # Errors
    /// [`StoreError`] as for [`PostgresStore::open`].
    pub fn reopen(self) -> Result<PostgresStore, StoreError> {
        let mut store = PostgresStore::open(&self.config)?.with_flaw(self.flaw);
        store.clock = self.clock;
        Ok(store)
    }

    /// The text the liveness lock of `generation` is derived from.
    fn liveness_key(&self, generation: u64) -> String {
        format!("kbf-state/holder/{}/{generation}", self.config.schema)
    }

    fn over(&self) -> Option<i64> {
        self.clock.as_ref().map(|clock| from_time(clock.now()))
    }

    fn count(&self, results: &[Result<Commit, Conflict>], wrote: bool) {
        let applied = results.iter().filter(|result| result.is_ok()).count();
        self.txns.fetch_add(applied as u64, Ordering::Relaxed);
        if wrote {
            self.syncs.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn read<T>(
        &self,
        f: impl FnOnce(&mut Session) -> Result<T, postgres::Error>,
    ) -> Result<T, StoreError> {
        let mut session = self.pool.checkout().map_err(Fail::into_store)?;
        f(&mut session).map_err(failed)
    }

    /// Runs `attempt` until it is not rolled back for a serialization failure.
    fn retrying<T>(&self, mut attempt: impl FnMut() -> Result<T, Fail>) -> Result<T, StoreError> {
        for _ in 0..MAX_ATTEMPTS {
            match attempt() {
                Ok(done) => return Ok(done),
                Err(fail) if fail.retryable() => {
                    self.retries.fetch_add(1, Ordering::Relaxed);
                }
                Err(fail) => return Err(fail.into_store()),
            }
        }
        Err(StoreError::Failed(format!(
            "the transaction failed to serialize {MAX_ATTEMPTS} times"
        )))
    }

    fn start(&self, level: Level) -> Result<Tx<'_>, Fail> {
        let session = self.pool.checkout()?;
        Tx::begin(session, level, self.over(), self.flaw)
    }

    fn run_batch(&self, batch: &[Txn]) -> Result<(Tx<'_>, Vec<Result<Commit, Conflict>>), Fail> {
        let mut tx = self.start(Level::Serializable)?;
        let mut results = Vec::with_capacity(batch.len());
        for txn in batch {
            results.push(tx.apply(txn)?);
        }
        Ok((tx, results))
    }

    /// Runs `txn` to the point of commit and holds it there. Test support.
    ///
    /// # Errors
    /// [`StoreError`] when the database fails.
    #[doc(hidden)]
    pub fn begin(&self, txn: Txn) -> Result<Pending<'_>, StoreError> {
        let batch = [txn];
        let (tx, results) = self.retrying(|| self.run_batch(&batch))?;
        Ok(Pending {
            store: self,
            tx,
            results,
        })
    }

    /// Claims up to `limit` live rows under `prefix`, in key order, skipping rows another
    /// transaction holds. `make` turns each row into the transaction that claims it
    /// (typically: delete the queue entry on its version, write a lease); `None` passes
    /// the row over, and a transaction whose precondition fails is a lost race and is
    /// passed over too. All claims commit together.
    ///
    /// The transaction runs read committed: after the commit-head lock every statement
    /// reads the latest committed rows, so there is no serialization failure to retry.
    ///
    /// # Errors
    /// [`StoreError`] when the database fails; nothing is claimed then.
    pub fn claim(
        &self,
        prefix: &str,
        limit: usize,
        mut make: impl FnMut(&str, &Versioned) -> Option<Txn>,
    ) -> Result<Vec<Claimed>, StoreError> {
        self.claim_in(prefix, limit, &mut make)
            .map_err(Fail::into_store)
    }

    fn claim_in(
        &self,
        prefix: &str,
        limit: usize,
        make: &mut dyn FnMut(&str, &Versioned) -> Option<Txn>,
    ) -> Result<Vec<Claimed>, Fail> {
        let mut tx = self.start(Level::ReadCommitted)?;
        let take = i64::try_from(limit).unwrap_or(i64::MAX);
        let skip_locked = self.flaw != Flaw::ClaimBlocks;
        let rows = tx.claim_rows(prefix, take, skip_locked)?;
        for (key, row) in &rows {
            tx.remember(key.clone(), row.clone());
        }
        let mut claimed = Vec::new();
        for (key, row) in rows {
            let was = versioned(&row);
            let Some(txn) = make(&key, &was) else {
                continue;
            };
            if let Ok(commit) = tx.apply(&txn)? {
                claimed.push(Claimed { key, was, commit });
            }
        }
        let wrote = tx.dirty();
        tx.commit()?;
        if wrote {
            self.syncs.fetch_add(1, Ordering::Relaxed);
        }
        self.txns.fetch_add(claimed.len() as u64, Ordering::Relaxed);
        Ok(claimed)
    }
}

impl Store for PostgresStore {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        if batch.is_empty() {
            return Ok(Vec::new());
        }
        // The commit is inside the retry: a serializable transaction can also be refused
        // when it commits.
        self.retrying(|| {
            let (tx, results) = self.run_batch(&batch)?;
            let wrote = tx.dirty();
            tx.commit()?;
            self.count(&results, wrote);
            Ok(results)
        })
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        let over = self.over();
        self.read(|s| {
            let row = s.client.query_opt(&s.st.get, &[&key, &over])?;
            Ok(row.map(|row| versioned(&Row::from_row(&row, 0))))
        })
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        let over = self.over();
        self.read(|s| {
            let rows = s.client.query(&s.st.scan, &[&prefix, &over])?;
            Ok(rows
                .iter()
                .map(|row| (row.get(0), versioned(&Row::from_row(row, 1))))
                .collect())
        })
    }

    /// C8 in one statement, so the page is one snapshot of committed transactions. The
    /// statement for the span's shape asks for one row past the limit to learn whether a
    /// resume token is due.
    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        let Some(span) = Span::of(&scan) else {
            return Ok(ScanPage {
                rows: Vec::new(),
                resume: None,
            });
        };
        let shape = RangeShape {
            lower_included: span.lower.included,
            upper: Edge::of(&span.upper),
            reverse: scan.direction == Direction::Reverse,
        };
        let over = self.over();
        let take = i64::try_from(scan.limit.get().saturating_add(1)).unwrap_or(i64::MAX);
        let mut params: Vec<&(dyn ToSql + Sync)> = vec![&over, &take, &span.lower.key];
        if let Bound::Included(key) | Bound::Excluded(key) = &span.upper {
            params.push(key);
        }
        let rows = self.read(|s| s.client.query(&s.st.ranges[shape.index()], &params))?;
        let rows = rows
            .iter()
            .map(|row| (row.get(0), versioned(&Row::from_row(row, 1))))
            .collect();
        Ok(page_of(rows, scan.limit))
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        let sequence = self.flaw == Flaw::SequenceRevisions;
        let after_i = i64::try_from(after.0).unwrap_or(i64::MAX);
        let take = i64::try_from(limit).unwrap_or(i64::MAX);
        let (head, rows) = self.read(|s| {
            // The head first: every revision up to it has committed, so the rows read
            // next include all of them.
            let head: i64 = if sequence {
                i64::MAX
            } else {
                s.client.query_one(&s.st.head_read, &[])?.get(0)
            };
            let rows = s.client.query(&s.st.changes, &[&after_i, &head, &take])?;
            Ok((head, rows))
        })?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let rev: i64 = row.get(0);
            let kind: i16 = row.get(2);
            let value: Option<Vec<u8>> = row.get(3);
            let event = match kind {
                0 => Event::Put(value.unwrap_or_default()),
                1 => Event::Delete,
                2 => Event::Expire,
                other => {
                    return Err(StoreError::Failed(format!("unknown feed row kind {other}")));
                }
            };
            out.push(Change {
                revision: Revision(u64::try_from(rev).unwrap_or(0)),
                key: row.get(1),
                event,
            });
        }
        let last = out.last().map_or(after, |change| change.revision);
        // A page that is not full has reached the head; a full one resumes after its
        // last row.
        let cursor = if out.len() < limit && !sequence {
            Revision(u64::try_from(head).unwrap_or(0)).max(after)
        } else {
            last
        };
        Ok(Changes { rows: out, cursor })
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.retrying(|| {
            let mut tx = self.start(Level::Serializable)?;
            let reaped = tx.reap()?;
            let wrote = tx.dirty();
            tx.commit()?;
            if wrote {
                self.syncs.fetch_add(1, Ordering::Relaxed);
            }
            Ok(reaped)
        })
    }

    /// The check is the first statement after the commit-head lock, in the transaction
    /// that reaps: a take that holds the head waits it out and the check then sees the
    /// new generation.
    fn reap_expired_if(
        &self,
        key: &str,
        cond: Cond,
    ) -> Result<Result<usize, Conflict>, StoreError> {
        let check = Txn::new().check(key, cond);
        self.retrying(|| {
            let mut tx = self.start(Level::Serializable)?;
            let verdict = match tx.apply(&check)? {
                Ok(_) => Ok(tx.reap()?),
                Err(conflict) => Err(conflict),
            };
            let wrote = tx.dirty();
            tx.commit()?;
            if wrote {
                self.syncs.fetch_add(1, Ordering::Relaxed);
            }
            Ok(verdict)
        })
    }

    /// A session-level advisory lock on a connection of its own, held until the guard is
    /// dropped or the server sees the connection die. Locks are per database, so the key
    /// names the schema and the generation.
    fn hold_liveness(&self, generation: u64) -> Result<LivenessGuard, StoreError> {
        let key = self.liveness_key(generation);
        let mut pg = self.config.pg.clone();
        let app = format!(
            "{}-holder",
            self.config.pg.get_application_name().unwrap_or("kbf-state")
        );
        pg.application_name(&app);
        // A silent network loss is noticed by the OS, not only by the next query.
        pg.keepalives(true).keepalives_idle(Duration::from_secs(30));
        let mut client = pg.connect(NoTls).map_err(failed)?;
        let mut got = false;
        for attempt in 0..HOLD_ATTEMPTS {
            if attempt > 0 {
                std::thread::sleep(HOLD_BACKOFF);
            }
            got = client
                .query_one(
                    "SELECT pg_try_advisory_lock(hashtextextended($1, 0))",
                    &[&key],
                )
                .map_err(failed)?
                .get(0);
            if got {
                break;
            }
        }
        if !got {
            return Err(StoreError::Failed(format!(
                "generation {generation} already has a live holder"
            )));
        }
        Ok(LivenessGuard::new(Holder(Mutex::new(client))))
    }

    /// Tries the same lock from a pooled connection for the length of one statement
    /// (transaction level, so it cannot be left behind): got it means no session holds it.
    fn holder_alive(&self, generation: u64) -> Result<bool, StoreError> {
        let key = self.liveness_key(generation);
        let free = self.read(|s| {
            let row = s.client.query_one(
                "SELECT pg_try_advisory_xact_lock(hashtextextended($1, 0))",
                &[&key],
            )?;
            Ok(row.get::<_, bool>(0))
        })?;
        Ok(!free)
    }

    fn commit_stats(&self) -> CommitStats {
        CommitStats {
            txns: self.txns.load(Ordering::Relaxed),
            syncs: self.syncs.load(Ordering::Relaxed),
        }
    }

    fn now(&self) -> StoreTime {
        if let Some(clock) = &self.clock {
            return clock.now();
        }
        // The store trait cannot report a failure here; a database that cannot be
        // reached reads as the origin, and the next call that can fail says why.
        self.read(|s| {
            let us: i64 = s.client.query_one(&s.st.now, &[&None::<i64>])?.get(0);
            Ok(to_time(us))
        })
        .unwrap_or(StoreTime::ZERO)
    }
}

#[cfg(test)]
mod tests {
    use postgres::config::Host;

    use std::time::Duration;

    use super::{
        Cond, ConfigError, Fail, MAX_ATTEMPTS, PostgresConfig, PostgresStore, Revision, Store,
        StoreError, Txn, describe,
    };

    // Catches a Unix-socket URI that parses to a TCP host (or not at all): the frontend
    // reaches Postgres over a socket directory, and `host=/dir` must be that directory.
    #[test]
    fn a_socket_uri_parses_to_a_unix_host() {
        let config = PostgresConfig::parse("postgres:///kbf?host=/run/postgresql")
            .expect("a socket directory is a host");
        assert_eq!(
            config.pg.get_hosts(),
            [Host::Unix("/run/postgresql".into())]
        );
        assert_eq!(config.pg.get_dbname(), Some("kbf"));
    }

    // Catches a URI with no host being accepted (it would connect to a default socket the
    // operator never chose).
    #[test]
    fn a_uri_with_no_host_is_refused() {
        assert_eq!(
            PostgresConfig::parse("postgres:///kbf").unwrap_err(),
            ConfigError::NoHost
        );
    }

    // Catches a URL with no application name leaving connections unnamed in
    // `pg_stat_activity`, and an application name the URL set being overwritten.
    #[test]
    fn a_url_without_an_application_name_gets_the_crates_and_keeps_its_own() {
        let plain = PostgresConfig::parse("postgres://h/db").expect("parses");
        assert_eq!(plain.pg.get_application_name(), Some("kbf-state"));
        let named = PostgresConfig::parse("postgres://h/db?application_name=mine").expect("parses");
        assert_eq!(named.pg.get_application_name(), Some("mine"));
    }

    // Catches a URL refused for the wrong reason (or accepted): each way `parse` says no
    // is its own error, and the answers that are yes keep the URL's own settings. All
    // here, in one test binary, so the function is covered whole by one copy.
    #[test]
    fn each_way_a_url_is_refused_is_its_own_error() {
        let refused = |url: &str| PostgresConfig::parse(url).unwrap_err();
        assert_eq!(refused(" "), ConfigError::Missing);
        assert_eq!(refused("mysql://h/db"), ConfigError::NotPostgres);
        assert!(matches!(
            refused("postgres://h/db?sslmode=bogus"),
            ConfigError::Invalid(_)
        ));
        assert_eq!(refused("postgres:///db"), ConfigError::NoHost);
        assert_eq!(
            refused("postgres://h/db?sslmode=require"),
            ConfigError::TlsUnsupported
        );
        assert!(PostgresConfig::parse("postgresql://h/db?sslmode=prefer").is_ok());
    }

    // Catches a pool size of zero (a store that could never check a session out) and a
    // size that is not kept.
    #[test]
    fn a_pool_has_at_least_one_connection() {
        let config = PostgresConfig::parse("postgres://h/db").expect("parses");
        assert_eq!(config.clone().with_max_connections(0).max_connections, 1);
        assert_eq!(config.with_max_connections(5).max_connections, 5);
    }

    // Catches a schema-name check that lets through a name Postgres truncates (over 63
    // bytes) or one that starts with a digit.
    #[test]
    fn a_schema_name_is_plain_and_at_most_63_bytes() {
        let config = PostgresConfig::parse("postgres://h/db").expect("parses");
        let long = "a".repeat(64);
        assert!(config.clone().with_schema(&"a".repeat(63)).is_ok());
        assert_eq!(
            config.clone().with_schema(&long).unwrap_err(),
            ConfigError::BadSchema(long)
        );
        assert!(config.clone().with_schema("1abc").is_err());
        assert!(config.with_schema("Abc").is_err());
    }

    // Catches a URL that carries a password being reported as one without (the flag
    // refuses a password), and one without being reported as having one.
    #[test]
    fn the_config_knows_whether_the_url_carries_a_password() {
        assert!(
            PostgresConfig::parse("postgres://u:pw@h/db")
                .expect("parses")
                .has_password()
        );
        assert!(
            !PostgresConfig::parse("postgres://u@h/db")
                .expect("parses")
                .has_password()
        );
    }

    // Catches a refusal the store made itself (not the server) being retried, and its
    // text being replaced by a generic one.
    #[test]
    fn a_refusal_is_not_retried_and_keeps_its_words() {
        let refusal = Fail::Refused("synchronous_commit is off".to_owned());
        assert!(!refusal.retryable());
        assert_eq!(
            refusal.into_store(),
            StoreError::Failed("synchronous_commit is off".to_owned())
        );
    }

    fn url() -> String {
        std::env::var("KBF_TEST_POSTGRES_URL")
            .expect("KBF_TEST_POSTGRES_URL names a database the tests may create schemas in")
    }

    /// A store on a schema of its own, and a client on the same database.
    fn store_and_client(schema: &str) -> (PostgresStore, postgres::Client) {
        let config = PostgresConfig::parse(&url())
            .and_then(|config| config.with_schema(schema))
            .expect("a usable URL and schema");
        let mut client = postgres::Client::connect(&url(), postgres::NoTls).expect("connect");
        client
            .batch_execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
            .expect("a clean schema");
        installed(&config);
        (PostgresStore::open(&config).expect("opens"), client)
    }

    /// The schema in `config`, as the installer makes it.
    fn installed(config: &PostgresConfig) {
        postgres::Client::connect(&url(), postgres::NoTls)
            .and_then(|mut client| {
                client.batch_execute(&format!(
                    "DROP SCHEMA IF EXISTS {} CASCADE",
                    config.schema()
                ))
            })
            .expect("a clean schema");
        super::schema::apply(config, &super::schema::embedded(), super::schema::latest())
            .expect("the installer's step");
    }

    /// The error the server gives for `code`.
    fn raised(client: &mut postgres::Client, code: &str) -> postgres::Error {
        client
            .batch_execute(&format!(
                "DO $$ BEGIN RAISE EXCEPTION 'raised' USING ERRCODE = '{code}'; END $$"
            ))
            .expect_err("the block raises")
    }

    // Catches a serialization failure or a deadlock not being retried (the store would
    // fail a transaction the server only asked it to run again), a failure that is
    // neither being retried, and a retry loop with no end. The server's words name the
    // state.
    #[test]
    #[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
    fn only_serialization_failures_and_deadlocks_are_run_again_and_not_for_ever() {
        let (store, mut client) = store_and_client("kbf_unit_retry");
        // One result type throughout: the coverage of a generic function is that of its
        // best single instantiation.
        let tries = std::cell::Cell::new(0);
        let mut run = |codes: &[&str]| -> Result<(), StoreError> {
            tries.set(0);
            store.retrying(|| {
                tries.set(tries.get() + 1);
                match codes.get(tries.get() - 1) {
                    Some(code) => Err(Fail::Pg(raised(&mut client, code))),
                    None => Ok(()),
                }
            })
        };
        for code in ["40001", "40P01"] {
            assert_eq!(
                run(&[code, code]),
                Ok(()),
                "{code}: run again until it works"
            );
        }
        assert_eq!(tries.get(), 3, "two failures, then the run that worked");
        let refused = run(&["23505", "40001"]);
        assert_eq!(tries.get(), 1, "a unique violation is not run again");
        let said = format!("{refused:?}");
        assert!(said.contains("SQLSTATE 23505"), "{said}");
        let endless = run(&["40001"; MAX_ATTEMPTS + 1]);
        assert_eq!(tries.get(), MAX_ATTEMPTS);
        assert_eq!(
            endless,
            Err(StoreError::Failed(format!(
                "the transaction failed to serialize {MAX_ATTEMPTS} times"
            )))
        );
    }

    // Catches an error that is not the server's (no connection, a closed one) being
    // described as the empty "db error", or with its cause dropped.
    #[test]
    #[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
    fn an_error_that_is_not_the_servers_is_described_with_its_cause() {
        let refused = postgres::Client::connect("postgres://nobody@127.0.0.1:1/x", postgres::NoTls)
            .err()
            .expect("nothing listens on port 1");
        assert!(refused.as_db_error().is_none());
        let said = describe(&refused);
        assert!(said.contains(": "), "{said}");
        let (_store, mut client) = store_and_client("kbf_unit_describe");
        let pid: i32 = client
            .query_one("SELECT pg_backend_pid()", &[])
            .expect("pid")
            .get(0);
        let mut killer = postgres::Client::connect(&url(), postgres::NoTls).expect("connect");
        killer
            .execute("SELECT pg_terminate_backend($1)", &[&pid])
            .expect("terminates");
        let closed = (0..2000)
            .find_map(|_| {
                std::thread::sleep(Duration::from_millis(5));
                client
                    .batch_execute("SELECT 1")
                    .err()
                    .filter(|e| e.as_db_error().is_none())
            })
            .expect("the dead connection fails without a database error");
        assert!(!describe(&closed).is_empty());
        assert_eq!(
            describe(&closed),
            closed.to_string(),
            "no cause to add: {closed:?}"
        );
    }

    // Catches an empty batch reaching for a connection: it must answer nothing without
    // one (the store's sessions are all dead here, and a batch that begins a transaction
    // would fail).
    #[test]
    #[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
    fn an_empty_batch_answers_nothing_without_a_connection() {
        let config = PostgresConfig::parse(&url())
            .and_then(|config| config.with_schema("kbf_unit_empty"))
            .expect("a usable URL and schema")
            .with_application_name("kbf_unit_empty");
        installed(&config);
        let store = PostgresStore::open(&config).expect("opens");
        let mut client = postgres::Client::connect(&url(), postgres::NoTls).expect("connect");
        let sessions = |client: &mut postgres::Client| -> i64 {
            client
                .query_one(
                    "SELECT count(*) FROM pg_stat_activity WHERE application_name = 'kbf_unit_empty'",
                    &[],
                )
                .expect("the activity view")
                .get(0)
        };
        client
            .execute(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                 WHERE application_name = 'kbf_unit_empty'",
                &[],
            )
            .expect("terminates");
        let gone = (0..2000).any(|_| {
            std::thread::sleep(Duration::from_millis(5));
            sessions(&mut client) == 0
        });
        assert!(gone, "the killed sessions go");
        assert!(store.apply_batch(Vec::new()).expect("answers").is_empty());
        // The same call with something in it needs a session: the dead one fails the
        // call, and the next call runs on a new connection. (Both ends of the function
        // run in this one test binary.)
        let put = || vec![Txn::new().put("k", "v", Cond::Any)];
        assert!(store.apply_batch(put()).is_err());
        assert_eq!(store.apply_batch(put()).expect("commits").len(), 1);
    }

    // Catches the feed read wrongly: each kind of row mapped to its event, a full page
    // resuming after its last row and a short one at the head, a store with sequence
    // revisions reading to the end of the table, and a row of a kind this code does not
    // know being read as an event (the feed would lose a change): it must fail and name
    // the kind.
    #[test]
    #[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
    fn the_feed_maps_each_kind_pages_and_refuses_a_kind_it_does_not_know() {
        use super::{Change, Event, Flaw};
        let (store, mut client) = store_and_client("kbf_unit_kind");
        client
            .batch_execute(
                "INSERT INTO kbf_unit_kind.feed (rev, key, kind, value) VALUES \
                 (1, 'a', 0, 'v'), (2, 'b', 1, NULL), (3, 'c', 2, NULL); \
                 UPDATE kbf_unit_kind.head SET rev = 5",
            )
            .expect("one row of each kind");
        let all = store.changes(Revision::ZERO, 10).expect("reads");
        assert_eq!(
            all.rows,
            [
                Change {
                    revision: Revision(1),
                    key: "a".into(),
                    event: Event::Put(b"v".to_vec())
                },
                Change {
                    revision: Revision(2),
                    key: "b".into(),
                    event: Event::Delete
                },
                Change {
                    revision: Revision(3),
                    key: "c".into(),
                    event: Event::Expire
                },
            ]
        );
        assert_eq!(
            all.cursor,
            Revision(5),
            "a short page ends at the head, not its last row"
        );
        let first = store.changes(Revision::ZERO, 1).expect("reads");
        assert_eq!((first.rows.len(), first.cursor), (1, Revision(1)));
        let flawed = store.with_flaw(Flaw::SequenceRevisions);
        assert_eq!(
            flawed
                .changes(Revision::ZERO, 10)
                .expect("reads")
                .rows
                .len(),
            3
        );
        client
            .batch_execute(
                "INSERT INTO kbf_unit_kind.feed (rev, key, kind) VALUES (6, 'k', 9); \
                 UPDATE kbf_unit_kind.head SET rev = 6",
            )
            .expect("a row of a kind from the future");
        assert_eq!(
            flawed
                .with_flaw(Flaw::None)
                .changes(Revision(3), 10)
                .unwrap_err(),
            StoreError::Failed("unknown feed row kind 9".to_owned())
        );
    }
}
