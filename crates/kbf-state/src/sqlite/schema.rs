//! The file's layout, the settings every connection takes, and the epoch.
//!
//! The layout changes by expansion only: a release adds a table, or a column that is
//! nullable or has a default, and never drops, renames or reinterprets what an older
//! release reads. So a file opens when its `user_version` is this binary's or newer and
//! every table and column in `NEEDED` exists; whatever else it holds (a column or table a
//! newer release added) is ignored, and `user_version` is left as the newer release set
//! it. The check is by name and does not look at column types. A file with an older
//! `user_version` than this binary's is refused today (no older version exists); a release
//! that raises `SCHEMA_VERSION` adds its migration in `init`. A file that lacks a table or
//! column the binary needs, or is another application's, is refused.

use std::time::Duration;

use rusqlite::{Connection, Row, Transaction, TransactionBehavior};

use super::SqliteError;

/// The `user_version` of a file this binary creates (`kbf --version --json` reports it
/// as `state_schema`). A file of a newer version opens too, as the module docs say.
pub const SCHEMA_VERSION: i64 = 1;

/// How long a connection waits for a lock another connection holds (a checkpoint).
pub(super) const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The feed row kinds, stored as integers.
pub(super) const KIND_PUT: i64 = 0;
pub(super) const KIND_DELETE: i64 = 1;
pub(super) const KIND_EXPIRE: i64 = 2;

/// `kv` holds the live keys (a version is the revision that wrote the value, an expiry is
/// nanoseconds on the store's clock), `feed` the change rows by revision, `counter` the
/// counters, and `meta` the head revision and the epoch.
const SCHEMA: &str = "
CREATE TABLE kv (
    key TEXT PRIMARY KEY NOT NULL,
    value BLOB NOT NULL,
    version INTEGER NOT NULL,
    expires_at INTEGER
) WITHOUT ROWID;
CREATE INDEX kv_expiry ON kv (expires_at) WHERE expires_at IS NOT NULL;
CREATE TABLE counter (
    name TEXT PRIMARY KEY NOT NULL,
    value INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE feed (
    revision INTEGER PRIMARY KEY NOT NULL,
    key TEXT NOT NULL,
    kind INTEGER NOT NULL,
    value BLOB
);
CREATE TABLE meta (
    name TEXT PRIMARY KEY NOT NULL,
    value INTEGER NOT NULL
) WITHOUT ROWID;
INSERT INTO meta (name, value) VALUES ('head', 0), ('epoch', 0);
";

/// The tables and columns this binary reads and writes. Its statements name their columns,
/// so a column a newer release adds to one of these tables must be nullable or have a
/// default. An overwrite of a `kv` key updates only the columns named here, so a column a
/// newer release added keeps its value.
const NEEDED: &[(&str, &[&str])] = &[
    ("kv", &["key", "value", "version", "expires_at"]),
    ("counter", &["name", "value"]),
    ("feed", &["revision", "key", "kind", "value"]),
    ("meta", &["name", "value"]),
];

/// The first column of a row, as an integer. The row readers are named functions rather
/// than closures so that each statement that uses one is a single covered line.
pub(super) fn first_i64(row: &Row<'_>) -> rusqlite::Result<i64> {
    row.get(0)
}

/// The first column of a row, as text.
pub(super) fn first_string(row: &Row<'_>) -> rusqlite::Result<String> {
    row.get(0)
}

/// The head revision the file records.
pub(super) fn head(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT value FROM meta WHERE name = 'head'", [], first_i64)
}

/// Puts a connection in WAL mode with a sync on every commit (a commit is acknowledged
/// only once it is on disk).
pub(super) fn configure(conn: &Connection) -> Result<(), SqliteError> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], first_string)?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(SqliteError::NotAStateStore(format!(
            "the filesystem cannot keep a WAL-mode database (journal mode {mode})"
        )));
    }
    conn.pragma_update(None, "synchronous", "FULL")?;
    Ok(())
}

/// Whether the table `table` has a column `column`. A table that does not exist has none.
fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let sql = "SELECT count(*) FROM pragma_table_info(?1) WHERE name = ?2";
    let found = conn.query_row(sql, [table, column], first_i64)?;
    Ok(found > 0)
}

/// The refusal of a file that has no `column` in `table`.
fn missing(version: i64, table: &str, column: &str) -> SqliteError {
    SqliteError::NotAStateStore(format!(
        "not a kbf state store this binary reads: version {version} has no {table}.{column}"
    ))
}

/// Accepts a file of schema `version` (this binary's or a newer one) that has every table
/// and column in `NEEDED`, extra ones included.
fn require_layout(conn: &Connection, version: i64) -> Result<(), SqliteError> {
    for &(table, columns) in NEEDED {
        for &column in columns {
            if !has_column(conn, table, column)? {
                return Err(missing(version, table, column));
            }
        }
    }
    Ok(())
}

/// Creates the layout in an empty file, accepts a file of this binary's schema or a newer
/// one that has what the binary needs, and refuses any other file.
pub(super) fn init(conn: &Connection) -> Result<(), SqliteError> {
    let version: i64 = conn.pragma_query_value(None, "user_version", first_i64)?;
    if version >= SCHEMA_VERSION {
        return require_layout(conn, version);
    }
    let tables: i64 = conn.query_row("SELECT count(*) FROM sqlite_master", [], first_i64)?;
    if version != 0 || tables != 0 {
        return Err(SqliteError::NotAStateStore(format!(
            "not a kbf state store this binary reads (schema version {version}, {tables} objects)"
        )));
    }
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    tx.execute_batch(SCHEMA)?;
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}

/// Refuses a file that is not a state store this binary reads: one of an older schema
/// than the binary's, or one that lacks a table or column the binary needs. A newer
/// schema that only adds is accepted, as `init` does.
pub(super) fn require_state_store(conn: &Connection) -> Result<(), SqliteError> {
    let version: i64 = conn.pragma_query_value(None, "user_version", first_i64)?;
    if version >= SCHEMA_VERSION {
        return require_layout(conn, version);
    }
    Err(SqliteError::NotAStateStore(format!(
        "not a kbf state store this binary reads (schema version {version})"
    )))
}

/// The epoch the file records.
pub(super) fn epoch(conn: &Connection) -> Result<u64, SqliteError> {
    let epoch: i64 =
        conn.query_row("SELECT value FROM meta WHERE name = 'epoch'", [], first_i64)?;
    Ok(u64::try_from(epoch).unwrap_or_default())
}

/// Moves the epoch past both the one the file records and `floor`, durably, and returns
/// it. A restart passes zero; a restore passes the epoch of the file it replaces, so an
/// epoch is never issued twice.
pub(super) fn bump_epoch(conn: &Connection, floor: u64) -> Result<u64, SqliteError> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let next = epoch(&tx)?.max(floor).saturating_add(1);
    let stored = i64::try_from(next).unwrap_or(i64::MAX);
    tx.execute("UPDATE meta SET value = ?1 WHERE name = 'epoch'", [stored])?;
    tx.commit()?;
    Ok(next)
}
