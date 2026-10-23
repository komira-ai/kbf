//! The Postgres store: the checks before a copy, the copy, and reading it back.
//!
//! The writes are SQL of this crate's own. The store's `Store` API gives every write a new
//! revision, and a copy has to keep the version each row has, so it cannot be used to
//! write. The tables, the columns and the sequence are those of the store's first migration
//! (`kv`, `counters`, `head`, `rev_seq`), and a store whose schema is not the version this
//! tool was built for is refused before anything is written.

use kbf_state::postgres::PostgresConfig;
use kbf_state::postgres::schema::{self, SchemaError};
use postgres::{Client, GenericClient, NoTls, Row, Transaction};

use crate::digest::{self, Counter, Entry, Summary};
use crate::error::Error;
use crate::seams::{Seams, Stage};
use crate::source::Snapshot;

/// How many rows one INSERT carries.
const BATCH: usize = 500;

/// The most the clocks of this host and of the database may differ: 2 seconds, in
/// microseconds. Expiry times are copied as they are, so a clock that is off moves every
/// session and lease the copy carries.
pub const MAX_CLOCK_SKEW_US: u64 = 2_000_000;

/// Why a mismatch found before the commit leaves nothing behind.
pub const IN_TRANSACTION: &str = "before the commit; the transaction was rolled back";

/// What a mismatch found after the commit means for the target.
pub const AFTER_COMMIT: &str = "after the commit; the target holds a copy that does not match \
    and needs its schema dropped and set up again before another try";

/// Rows and counters in the store, its feed entries, and its head revision, added up: zero
/// for a store nothing has ever written to.
const HELD: &str = "SELECT (SELECT count(*) FROM kv) + (SELECT count(*) FROM feed) \
    + (SELECT count(*) FROM counters) + (SELECT rev FROM head)";

/// The database's clock in microseconds on the Unix clock, as the store reads it.
const CLOCK: &str = "SELECT floor(extract(epoch from clock_timestamp()) * 1000000)::bigint";

/// The longest the copy waits for a lock on the store and for any one statement. A longer
/// wait fails the copy, which rolls back.
const WAITS: &str = "SET LOCAL lock_timeout = '10s'; SET LOCAL statement_timeout = '300s'";

const READ_HEAD: &str = "SELECT rev FROM head";

const INSERT_KV: &str = "INSERT INTO kv (key, value, version, expires_us) \
    SELECT * FROM unnest($1::text[], $2::bytea[], $3::bigint[], $4::bigint[])";

const INSERT_COUNTERS: &str = "INSERT INTO counters (name, value) \
    SELECT * FROM unnest($1::text[], $2::bigint[])";

/// Sets the schema's revision sequence to `$1`, so that its next value is above it. The
/// store takes its revisions from the head row, which is set beside the sequence; a head
/// of 0 leaves the sequence where it starts.
const SEQUENCE: &str = "SELECT setval('rev_seq', GREATEST($1::bigint, 1), $1::bigint > 0)";

const READ_KV: &str = "SELECT key, value, version, expires_us FROM kv ORDER BY key";
const READ_COUNTERS: &str = "SELECT name, value FROM counters ORDER BY name";

/// A Postgres store to copy into: the database a URL names, in the store's own schema.
pub struct Target {
    url: String,
    config: PostgresConfig,
}

/// What a dry run learns about the target.
pub struct Probe {
    /// What the store holds, added up (see [`held_rows`]).
    pub held: i64,
    /// The database's clock, in microseconds on the Unix clock.
    pub server_clock_us: i64,
}

impl Target {
    /// The target `url` names.
    ///
    /// # Errors
    /// [`Error::Failed`] when the URL is not one the store accepts.
    pub fn new(url: &str) -> Result<Target, Error> {
        let config = PostgresConfig::parse(url)?;
        Ok(Target {
            url: url.trim().to_owned(),
            config,
        })
    }

    /// Opens a connection that sees the store's tables. Reaching the server takes at most
    /// [`schema::CONNECT_LIMIT`].
    ///
    /// # Errors
    /// [`Error::Failed`] when the server cannot be reached or refuses the connection.
    pub fn connect(&self) -> Result<Client, Error> {
        let mut settings: postgres::Config = self.url.parse()?;
        settings.connect_timeout(schema::CONNECT_LIMIT);
        let mut client = settings.connect(NoTls)?;
        let name = self.config.schema();
        client.batch_execute(&format!("SET search_path TO {name}"))?;
        Ok(client)
    }

    /// Refuses a store whose schema is not the version this tool was built for.
    ///
    /// # Errors
    /// [`Error::Refused`] when the schema is missing, older or newer; [`Error::Failed`]
    /// when the server cannot be reached.
    pub fn check_schema(&self) -> Result<(), Error> {
        let found = match schema::current(&self.config) {
            Ok(found) => found,
            Err(SchemaError::NotInitialised) => 0,
            Err(other) => return Err(Error::Failed(other.to_string())),
        };
        let latest = schema::latest();
        if found == latest {
            return Ok(());
        }
        Err(Error::Refused(format!(
            "the schema is at version {found} (0 means none) and this tool copies into \
             version {latest}: set it up with the host's setup, or use the tool of the \
             release that matches"
        )))
    }

    /// What the store holds and what time it is there, read without changing anything.
    ///
    /// # Errors
    /// [`Error::Failed`] when a call fails.
    pub fn probe(&self) -> Result<Probe, Error> {
        let mut client = self.connect()?;
        let held = held_rows(&mut client)?;
        let server_clock_us = client.query_one(CLOCK, &[])?.get(0);
        Ok(Probe {
            held,
            server_clock_us,
        })
    }

    /// Writes `snap` in one transaction: takes the same lock a writer takes on the commit
    /// head (a frontend that starts meanwhile waits; the copy waits at most 10 s for it, and
    /// 300 s for any one statement), refuses a store that is not empty, writes the rows and
    /// the counters with their versions, moves the head past the highest version, reads the
    /// rows and the head back inside the transaction and compares them with `want` and the
    /// snapshot, then moves the revision sequence (a sequence is not rolled back, so it is
    /// the last write), and only then commits. Any failure drops the transaction, which
    /// rolls it back.
    ///
    /// # Errors
    /// [`Error::Refused`] when the store is not empty; [`Error::Failed`] when a call fails
    /// or the rows or the head read back do not match.
    pub fn apply(&self, snap: &Snapshot, want: &Summary, seams: &Seams) -> Result<(), Error> {
        let mut client = self.connect()?;
        let mut tx = client.transaction()?;
        tx.batch_execute(WAITS)?;
        tx.batch_execute("LOCK TABLE head IN SHARE UPDATE EXCLUSIVE MODE")?;
        tx.batch_execute("SELECT rev FROM head FOR UPDATE")?;
        require_empty(held_rows(&mut tx)?)?;
        insert_entries(&mut tx, &snap.entries)?;
        insert_counters(&mut tx, &snap.counters)?;
        let head = snap.target_head();
        tx.execute("UPDATE head SET rev = $1", &[&head])?;
        if let Some(statement) = (seams.tamper)(Stage::InTransaction) {
            tx.batch_execute(statement)?;
        }
        verify(want, &summary(&mut tx)?, IN_TRANSACTION)?;
        verify_head(head, read_head(&mut tx)?, IN_TRANSACTION)?;
        tx.query_one(SEQUENCE, &[&head])?;
        tx.commit()?;
        Ok(())
    }
}

/// What the store holds, added up: key/value rows, feed entries and counters, and the head
/// revision. Zero only for a store nothing has written to.
///
/// # Errors
/// [`Error::Failed`] when the query fails.
pub fn held_rows(client: &mut impl GenericClient) -> Result<i64, Error> {
    Ok(client.query_one(HELD, &[])?.get(0))
}

/// Refuses a store that holds anything.
///
/// # Errors
/// [`Error::Refused`] when `held` is not zero.
pub fn require_empty(held: i64) -> Result<(), Error> {
    if held == 0 {
        return Ok(());
    }
    let text = "the target already holds data (rows, feed entries, counters or a revision)";
    Err(Error::Refused(text.to_owned()))
}

/// Refuses clocks that are more than [`MAX_CLOCK_SKEW_US`] apart.
///
/// # Errors
/// [`Error::Refused`] when they are.
pub fn require_clocks(server_us: i64, local_us: i64) -> Result<(), Error> {
    let apart = server_us.abs_diff(local_us);
    if apart <= MAX_CLOCK_SKEW_US {
        return Ok(());
    }
    let seconds = apart / 1_000_000;
    Err(Error::Refused(format!(
        "this host's clock and the database's differ by {seconds} s; the limit is 2 s"
    )))
}

/// The families of the rows and counters the store holds.
///
/// # Errors
/// [`Error::Failed`] when a query fails.
pub fn summary(client: &mut impl GenericClient) -> Result<Summary, Error> {
    let rows = client.query(READ_KV, &[])?;
    let entries: Vec<Entry> = rows.iter().map(read_entry).collect();
    let rows = client.query(READ_COUNTERS, &[])?;
    let counters: Vec<Counter> = rows.iter().map(read_counter).collect();
    Ok(digest::summarize(&entries, &counters))
}

/// Compares what was read back (`got`) with the state file (`want`). `when` says what a
/// mismatch means at this point.
///
/// # Errors
/// [`Error::Failed`] naming each family that differs.
pub fn verify(want: &Summary, got: &Summary, when: &str) -> Result<(), Error> {
    let problems = want.differences(got);
    if problems.is_empty() {
        return Ok(());
    }
    let list = problems.join("; ");
    Err(Error::Failed(format!("the copy does not match the state file {when}: {list}")))
}

/// The head revision the store holds.
///
/// # Errors
/// [`Error::Failed`] when the query fails.
pub fn read_head(client: &mut impl GenericClient) -> Result<i64, Error> {
    Ok(client.query_one(READ_HEAD, &[])?.get(0))
}

/// Compares the head revision that was read back (`got`) with the one the copy set (`want`).
/// `when` says what a mismatch means at this point.
///
/// # Errors
/// [`Error::Failed`] when they differ.
pub fn verify_head(want: i64, got: i64, when: &str) -> Result<(), Error> {
    if got == want {
        return Ok(());
    }
    Err(Error::Failed(format!(
        "the copy does not match the state file {when}: the head revision is {got}, not {want}"
    )))
}

fn insert_entries(tx: &mut Transaction<'_>, entries: &[Entry]) -> Result<(), Error> {
    for chunk in entries.chunks(BATCH) {
        let keys: Vec<&str> = chunk.iter().map(|e| e.key.as_str()).collect();
        let values: Vec<&[u8]> = chunk.iter().map(|e| e.value.as_slice()).collect();
        let versions: Vec<i64> = chunk.iter().map(|e| e.version).collect();
        let expiries: Vec<Option<i64>> = chunk.iter().map(|e| e.expires_us).collect();
        tx.execute(INSERT_KV, &[&keys, &values, &versions, &expiries])?;
    }
    Ok(())
}

fn insert_counters(tx: &mut Transaction<'_>, counters: &[Counter]) -> Result<(), Error> {
    for chunk in counters.chunks(BATCH) {
        let names: Vec<&str> = chunk.iter().map(|c| c.name.as_str()).collect();
        let values: Vec<i64> = chunk.iter().map(|c| c.value).collect();
        tx.execute(INSERT_COUNTERS, &[&names, &values])?;
    }
    Ok(())
}

fn read_entry(row: &Row) -> Entry {
    Entry {
        key: row.get(0),
        value: row.get(1),
        version: row.get(2),
        expires_us: row.get(3),
    }
}

fn read_counter(row: &Row) -> Counter {
    Counter {
        name: row.get(0),
        value: row.get(1),
    }
}
