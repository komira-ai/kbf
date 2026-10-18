//! Connections: a small pool of sessions, each with its statements prepared once.
//!
//! Every session is checked when it connects: the server must make a commit durable
//! before it acknowledges it (`synchronous_commit` on), or the store refuses to run.

use std::ops::{Bound, Deref, DerefMut};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

use postgres::{Config, Statement};

use super::Fail;
use super::conn::Conn;
use super::schema::{SchemaError, Supported, check_conn, require_durable};

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
    /// C8, one statement per [`RangeShape`] at [`RangeShape::index`]: `$1` clock override,
    /// `$2` limit, `$3` the lower key, `$4` the upper key when that side is bounded: `key,
    /// value, version, expires_us` of the live keys in the span, in the shape's direction.
    pub ranges: Vec<Statement>,
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
    fn prepare(client: &mut Conn) -> Result<Statements, postgres::Error> {
        let now = now_sql(2);
        let claim = |tail: &str| {
            format!(
                "SELECT key, value, version, expires_us FROM kv \
                 WHERE key >= $1 AND starts_with(key, $1) \
                 AND (expires_us IS NULL OR expires_us > $2) \
                 ORDER BY key LIMIT $3 FOR UPDATE{tail}"
            )
        };
        let ranges = RangeShape::ALL
            .iter()
            .map(|shape| client.prepare(&shape.sql(&now_sql(1))))
            .collect::<Result<Vec<_>, _>>()?;
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
            ranges,
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

/// How the upper side of a C8 span is bounded. (The lower side is always a key, so it
/// is only included or excluded: [`RangeShape::lower_included`].)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Edge {
    Included,
    Excluded,
    Open,
}

impl Edge {
    const ALL: [Edge; 3] = [Edge::Included, Edge::Excluded, Edge::Open];

    pub(super) fn of<T>(bound: &Bound<T>) -> Edge {
        match bound {
            Bound::Included(_) => Edge::Included,
            Bound::Excluded(_) => Edge::Excluded,
            Bound::Unbounded => Edge::Open,
        }
    }
}

/// The shape of a C8 statement: whether the span's lower key is included, how its upper
/// side is bounded, and which way the scan walks. Each shape has its own prepared
/// statement, so the planner sees plain range conditions on the primary key and walks
/// its index from the right end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RangeShape {
    pub lower_included: bool,
    pub upper: Edge,
    pub reverse: bool,
}

impl RangeShape {
    const ALL: [RangeShape; 12] = {
        let mut all = [RangeShape {
            lower_included: true,
            upper: Edge::Open,
            reverse: false,
        }; 12];
        let mut i = 0;
        while i < 12 {
            all[i] = RangeShape {
                lower_included: i / 6 == 0,
                upper: Edge::ALL[i / 2 % 3],
                reverse: i % 2 == 1,
            };
            i += 1;
        }
        all
    };

    /// Where this shape's statement sits in [`Statements::ranges`]: the inverse of the
    /// order of [`RangeShape::ALL`].
    pub(super) fn index(self) -> usize {
        let upper = match self.upper {
            Edge::Included => 0,
            Edge::Excluded => 1,
            Edge::Open => 2,
        };
        usize::from(!self.lower_included) * 6 + upper * 2 + usize::from(self.reverse)
    }

    fn sql(self, now: &str) -> String {
        let lower = if self.lower_included { ">=" } else { ">" };
        let mut conditions = vec![
            format!("(expires_us IS NULL OR expires_us > {now})"),
            format!("key {lower} $3"),
        ];
        let upper = match self.upper {
            Edge::Included => Some("<="),
            Edge::Excluded => Some("<"),
            Edge::Open => None,
        };
        if let Some(op) = upper {
            conditions.push(format!("key {op} $4"));
        }
        let order = if self.reverse { "DESC" } else { "ASC" };
        format!(
            "SELECT key, value, version, expires_us FROM kv WHERE {} ORDER BY key {order} LIMIT $2",
            conditions.join(" AND ")
        )
    }
}

/// One connection and its prepared statements.
pub(super) struct Session {
    pub client: Conn,
    pub st: Statements,
    /// Set when the connection is in an unknown state; the pool drops it.
    pub broken: bool,
}

/// Opens a connection. The first one of a store (`judge` is `Some`) also refuses a schema
/// outside what this build supports; later ones do not, so a serving child keeps working
/// when the installer migrates under it.
fn connect(config: &Config, schema: &str, judge: Option<Supported>) -> Result<Session, Fail> {
    let mut client = Conn::connect(config)?;
    require_durable(&mut client).map_err(refuse)?;
    if let Some(supported) = judge {
        check_conn(&mut client, schema, supported).map_err(refuse)?;
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

fn refuse(err: SchemaError) -> Fail {
    Fail::Refused(err.to_string())
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
    /// Connects once, refusing a server that does not make commits durable and a schema
    /// outside `supported`. It creates nothing: the installer owns the schema.
    pub(super) fn open(
        config: Config,
        schema: String,
        max: usize,
        supported: Supported,
    ) -> Result<Pool, Fail> {
        let first = connect(&config, &schema, Some(supported))?;
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
                return match connect(&self.config, &self.schema, None) {
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

#[cfg(test)]
mod tests {
    use super::RangeShape;

    // Catches an index that does not invert the order of the prepared statements, which
    // would run a scan on another shape's statement (wrong bounds or direction), and a
    // repeated shape that would leave one shape with no statement.
    #[test]
    fn each_shape_indexes_its_own_statement() {
        for (place, shape) in RangeShape::ALL.iter().enumerate() {
            assert_eq!(shape.index(), place, "{shape:?}");
        }
    }
}
