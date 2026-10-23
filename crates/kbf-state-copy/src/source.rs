//! The SQLite state file, read through rusqlite alone.
//!
//! The file is opened read-only and *immutable* (`immutable=1`): SQLite takes no locks,
//! creates no `-shm` or `-wal` file beside it and cannot change it, so reading it leaves
//! no trace, not even a file owned by whoever ran the tool for the frontend to trip over
//! afterwards. An immutable open ignores the write-ahead log, so a log that holds writes
//! the file does not is refused (a frontend that stopped cleanly leaves none). The whole
//! read is one transaction: one snapshot.
//!
//! The layout is the one `kbf-state`'s SQLite store creates (`user_version` 1): `kv` (key,
//! value, version, `expires_at` in nanoseconds on the Unix clock), `counter` (name, value)
//! and `meta` (the head revision). The `feed` table is not read.

use std::fs;
use std::path::Path;

use rusqlite::{Connection, OpenFlags, Row, Transaction, TransactionBehavior};

use crate::digest::{self, Counter, Entry, Summary};
use crate::error::Error;
use crate::lock;

/// The `user_version` of the state file layout this tool reads.
const SCHEMA_VERSION: i64 = 1;

/// The state file keeps an expiry in nanoseconds; the Postgres store in microseconds.
const NANOS_PER_MICRO: i64 = 1000;

const HEAD: &str = "SELECT value FROM meta WHERE name = 'head'";
const ENTRIES: &str = "SELECT key, value, version, expires_at FROM kv ORDER BY key";
const COUNTERS: &str = "SELECT name, value FROM counter ORDER BY name";

/// What the state file holds.
#[derive(Debug)]
pub struct Snapshot {
    /// The key/value rows in key order, with their expiry in microseconds.
    pub entries: Vec<Entry>,
    /// The counters in name order.
    pub counters: Vec<Counter>,
    /// The head revision the file records.
    pub head: i64,
}

impl Snapshot {
    /// The highest version any key/value row holds, 0 when there is none.
    #[must_use]
    pub fn highest_version(&self) -> i64 {
        let versions = self.entries.iter().map(|entry| entry.version);
        versions.max().unwrap_or(0)
    }

    /// The head revision the target is left at: the file's recorded head, or the highest
    /// version when that is higher, so that no later write is given a revision a copied
    /// row already has.
    #[must_use]
    pub fn target_head(&self) -> i64 {
        self.head.max(self.highest_version())
    }

    /// The families of the file's rows and counters.
    #[must_use]
    pub fn summary(&self) -> Summary {
        digest::summarize(&self.entries, &self.counters)
    }
}

/// Reads the state file `db` (its canonical name) in one transaction.
///
/// # Errors
/// [`Error::Refused`] when the log beside the file holds writes the file does not, or the
/// file is not the layout this tool reads; [`Error::Failed`] when SQLite cannot open or
/// read it.
pub fn read(db: &Path) -> Result<Snapshot, Error> {
    refuse_unmerged_log(db)?;
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    let conn = Connection::open_with_flags(uri(db)?, flags)?;
    let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Deferred)?;
    let version: i64 = tx.pragma_query_value(None, "user_version", first)?;
    if version != SCHEMA_VERSION {
        return Err(Error::Refused(format!(
            "the state file is schema version {version}; this tool reads version {SCHEMA_VERSION}"
        )));
    }
    let head: i64 = tx.query_row(HEAD, [], first)?;
    let mut statement = tx.prepare(ENTRIES)?;
    let entries = statement.query_map([], entry)?;
    let entries = entries.collect::<Result<Vec<_>, _>>()?;
    let mut statement = tx.prepare(COUNTERS)?;
    let counters = statement.query_map([], counter)?;
    let counters = counters.collect::<Result<Vec<_>, _>>()?;
    Ok(Snapshot {
        entries,
        counters,
        head,
    })
}

/// The URI SQLite opens `db` by: read-only and immutable. `%`, `?` and `#` are escaped, the
/// characters a URI path gives a meaning to.
///
/// # Errors
/// [`Error::Failed`] when the path is not UTF-8.
pub fn uri(db: &Path) -> Result<String, Error> {
    let Some(text) = db.to_str() else {
        return Err(Error::Failed(format!(
            "{}: the path is not UTF-8",
            db.display()
        )));
    };
    let escaped = text
        .replace('%', "%25")
        .replace('?', "%3f")
        .replace('#', "%23");
    Ok(format!("file:{escaped}?immutable=1"))
}

/// Refuses a state file whose write-ahead log holds bytes: an immutable open would not
/// read them.
fn refuse_unmerged_log(db: &Path) -> Result<(), Error> {
    let log = lock::beside(db, "-wal");
    let bytes = fs::metadata(&log).map_or(0, |meta| meta.len());
    if bytes == 0 {
        return Ok(());
    }
    let name = log.display();
    Err(Error::Refused(format!(
        "{name} holds {bytes} bytes the state file does not: the frontend did not stop \
         cleanly; let it open the state file and stop cleanly, which folds the log in"
    )))
}

fn first(row: &Row<'_>) -> rusqlite::Result<i64> {
    row.get(0)
}

fn entry(row: &Row<'_>) -> rusqlite::Result<Entry> {
    let expires_ns: Option<i64> = row.get(3)?;
    Ok(Entry {
        key: row.get(0)?,
        value: row.get(1)?,
        version: row.get(2)?,
        expires_us: expires_ns.map(|ns| ns / NANOS_PER_MICRO),
    })
}

fn counter(row: &Row<'_>) -> rusqlite::Result<Counter> {
    Ok(Counter {
        name: row.get(0)?,
        value: row.get(1)?,
    })
}
