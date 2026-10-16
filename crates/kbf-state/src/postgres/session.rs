//! Connections: a small pool of sessions, each with its statements prepared once.
//!
//! Every session is checked when it connects: the server must make a commit durable
//! before it acknowledges it (`synchronous_commit` on), or the store refuses to run.

use std::ops::{Deref, DerefMut};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

use postgres::{Client, Config, NoTls, Statement};

use super::Fail;

/// The time of the store's clock in microseconds since the Unix epoch, as SQL: the value
/// of parameter `$n` when it is not NULL (a test's injected clock), else the database's
/// own `clock_timestamp()` (`now()` is the transaction's start time, which a long batch
/// would judge expiry by).
fn now_sql(n: u8) -> String {
    format!(
        "COALESCE(${n}::bigint, floor(extract(epoch from clock_timestamp()) * 1000000)::bigint)"
    )
}

/// The statements every session runs, prepared once per connection. Table names resolve
/// through the session's `search_path`, which is the store's schema.
pub(super) struct Statements {
    /// `$1` key, `$2` clock override: `value, version, expires_us` of a live key.
    pub get: Statement,
    /// `$1` prefix, `$2` clock override: `key, value, version, expires_us`, in key order.
    pub scan: Statement,
    /// `$1` prefix, `$2` clock override, `$3` first key, `$4` limit: `key, value,
    /// version, expires_us` from `$3` on, in key order (C8).
    pub scan_from: Statement,
    /// The same after `$3`, not at it.
    pub scan_after: Statement,
    /// `$1` clock override: the store's time.
    pub now: Statement,
    /// The commit head, read without a lock.
    pub head_read: Statement,
    /// The commit head, locked until the end of the transaction.
    pub head_lock: Statement,
    /// `$1` the new head.
    pub head_set: Statement,
    /// `$1` keys: their rows, locked.
    pub load: Statement,
    /// `$1` key, `$2` value, `$3` version, `$4` expiry.
    pub upsert: Statement,
    /// `$1` key.
    pub delete: Statement,
    /// `$1` revision, `$2` key, `$3` kind, `$4` value.
    pub feed: Statement,
    /// `$1` counter: its new value.
    pub counter: Statement,
    /// The next value of the revision sequence (only the sequence flaw uses it).
    pub seq: Statement,
    /// `$1` now: the keys the clock has expired, locked.
    pub due: Statement,
    /// `$1` after, `$2` upper bound, `$3` limit: feed rows.
    pub changes: Statement,
    /// `$1` prefix, `$2` now, `$3` limit: live rows under a prefix, locked, skipping rows
    /// another transaction holds.
    pub claim: Statement,
    /// The same without `SKIP LOCKED` (the claim flaw).
    pub claim_waiting: Statement,
}

impl Statements {
    fn prepare(client: &mut Client) -> Result<Statements, postgres::Error> {
        let now = now_sql(2);
        let claim = |tail: &str| {
            format!(
                "SELECT key, value, version, expires_us FROM kv \
                 WHERE key >= $1 AND starts_with(key, $1) \
                 AND (expires_us IS NULL OR expires_us > $2) \
                 ORDER BY key LIMIT $3 FOR UPDATE{tail}"
            )
        };
        let range = |op: &str| {
            format!(
                "SELECT key, value, version, expires_us FROM kv \
                 WHERE key {op} $3 AND starts_with(key, $1) \
                 AND (expires_us IS NULL OR expires_us > {now}) ORDER BY key LIMIT $4"
            )
        };
        Ok(Statements {
            get: client.prepare(&format!(
                "SELECT value, version, expires_us FROM kv WHERE key = $1 \
                 AND (expires_us IS NULL OR expires_us > {now})"
            ))?,
            scan: client.prepare(&format!(
                "SELECT key, value, version, expires_us FROM kv \
                 WHERE key >= $1 AND starts_with(key, $1) \
                 AND (expires_us IS NULL OR expires_us > {now}) ORDER BY key"
            ))?,
            scan_from: client.prepare(&range(">="))?,
            scan_after: client.prepare(&range(">"))?,
            now: client.prepare(&format!("SELECT {}", now_sql(1)))?,
            head_read: client.prepare("SELECT rev FROM head")?,
            head_lock: client.prepare("SELECT rev FROM head FOR UPDATE")?,
            head_set: client.prepare("UPDATE head SET rev = $1")?,
            load: client.prepare(
                "SELECT key, value, version, expires_us FROM kv WHERE key = ANY($1::text[]) FOR UPDATE",
            )?,
            upsert: client.prepare(
                "INSERT INTO kv (key, value, version, expires_us) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, \
                 version = EXCLUDED.version, expires_us = EXCLUDED.expires_us",
            )?,
            delete: client.prepare("DELETE FROM kv WHERE key = $1")?,
            feed: client
                .prepare("INSERT INTO feed (rev, key, kind, value) VALUES ($1, $2, $3, $4)")?,
            counter: client.prepare(
                "INSERT INTO counters (name, value) VALUES ($1, 1) \
                 ON CONFLICT (name) DO UPDATE SET value = counters.value + 1 RETURNING value",
            )?,
            seq: client.prepare("SELECT nextval('rev_seq')")?,
            due: client.prepare(
                "SELECT key FROM kv WHERE expires_us IS NOT NULL AND expires_us <= $1 \
                 ORDER BY key FOR UPDATE",
            )?,
            changes: client.prepare(
                "SELECT rev, key, kind, value FROM feed WHERE rev > $1 AND rev <= $2 \
                 ORDER BY rev LIMIT $3",
            )?,
            claim: client.prepare(&claim(" SKIP LOCKED"))?,
            claim_waiting: client.prepare(&claim(""))?,
        })
    }
}

/// One connection and its prepared statements.
pub(super) struct Session {
    pub client: Client,
    pub st: Statements,
    /// Set when the connection is in an unknown state; the pool drops it.
    pub broken: bool,
}

/// The tables of one store, in its schema. Created under an advisory lock, so several
/// frontends starting together do not race on the catalog.
fn schema_sql(schema: &str) -> String {
    format!(
        "BEGIN;
         SELECT pg_advisory_xact_lock(hashtext('kbf-state-schema'));
         CREATE SCHEMA IF NOT EXISTS {schema};
         CREATE TABLE IF NOT EXISTS {schema}.kv (
             key text COLLATE \"C\" PRIMARY KEY,
             value bytea NOT NULL,
             version bigint NOT NULL,
             expires_us bigint);
         CREATE INDEX IF NOT EXISTS kv_expiry ON {schema}.kv (expires_us)
             WHERE expires_us IS NOT NULL;
         CREATE TABLE IF NOT EXISTS {schema}.feed (
             rev bigint PRIMARY KEY,
             key text NOT NULL,
             kind smallint NOT NULL,
             value bytea);
         CREATE TABLE IF NOT EXISTS {schema}.counters (
             name text PRIMARY KEY,
             value bigint NOT NULL);
         CREATE TABLE IF NOT EXISTS {schema}.head (
             singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
             rev bigint NOT NULL);
         INSERT INTO {schema}.head (rev) VALUES (0) ON CONFLICT DO NOTHING;
         CREATE SEQUENCE IF NOT EXISTS {schema}.rev_seq;
         COMMIT;"
    )
}

fn connect(config: &Config, schema: &str, create: bool) -> Result<Session, Fail> {
    let mut client = config.connect(NoTls)?;
    let setting: String = client.query_one("SHOW synchronous_commit", &[])?.get(0);
    if !matches!(setting.as_str(), "on" | "remote_apply") {
        return Err(Fail::Refused(format!(
            "synchronous_commit is {setting:?} on this connection; the state store needs \
             \"on\" (or \"remote_apply\"), or an acknowledged write can be lost"
        )));
    }
    if create {
        client.batch_execute(&schema_sql(schema))?;
    }
    client.batch_execute(&format!(
        "SET search_path TO {schema}; SET default_transaction_isolation = 'read committed'"
    ))?;
    let st = Statements::prepare(&mut client)?;
    Ok(Session {
        client,
        st,
        broken: false,
    })
}

struct PoolState {
    idle: Vec<Session>,
    /// Sessions that exist: idle, checked out, or being connected.
    open: usize,
}

/// At most `max` sessions; a caller past that waits for one to come back.
pub(super) struct Pool {
    config: Config,
    schema: String,
    max: usize,
    state: Mutex<PoolState>,
    ready: Condvar,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Pool {
    /// Connects once, refusing a server that does not make commits durable, and creates
    /// the schema if it is not there.
    pub(super) fn open(config: Config, schema: String, max: usize) -> Result<Pool, Fail> {
        let first = connect(&config, &schema, true)?;
        Ok(Pool {
            config,
            schema,
            max,
            state: Mutex::new(PoolState {
                idle: vec![first],
                open: 1,
            }),
            ready: Condvar::new(),
        })
    }

    pub(super) fn checkout(&self) -> Result<Pooled<'_>, Fail> {
        let mut state = lock(&self.state);
        loop {
            if let Some(session) = state.idle.pop() {
                return Ok(Pooled {
                    pool: self,
                    session: Some(session),
                });
            }
            if state.open < self.max {
                state.open += 1;
                drop(state);
                return match connect(&self.config, &self.schema, false) {
                    Ok(session) => Ok(Pooled {
                        pool: self,
                        session: Some(session),
                    }),
                    Err(err) => {
                        self.forget_one();
                        Err(err)
                    }
                };
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn forget_one(&self) {
        lock(&self.state).open -= 1;
        self.ready.notify_one();
    }

    fn give_back(&self, session: Session) {
        if session.broken || session.client.is_closed() {
            self.forget_one();
        } else {
            lock(&self.state).idle.push(session);
            self.ready.notify_one();
        }
    }
}

/// A session on loan from the pool; it goes back on drop unless it is broken.
pub(super) struct Pooled<'p> {
    pool: &'p Pool,
    session: Option<Session>,
}

impl Deref for Pooled<'_> {
    type Target = Session;

    fn deref(&self) -> &Session {
        // The session is taken only in `drop`.
        self.session
            .as_ref()
            .expect("a pooled session is present until it is dropped")
    }
}

impl DerefMut for Pooled<'_> {
    fn deref_mut(&mut self) -> &mut Session {
        self.session
            .as_mut()
            .expect("a pooled session is present until it is dropped")
    }
}

impl Drop for Pooled<'_> {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            self.pool.give_back(session);
        }
    }
}
