//! The read side: queries on a connection of their own, which in WAL mode never wait
//! for the writer and never see a transaction that has not committed.

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};

use super::schema::{KIND_DELETE, KIND_PUT};
use super::writer::{from_nanos, to_nanos, uint};
use crate::clock::StoreTime;
use crate::types::{Change, Changes, Event, Revision, StoreError, Version, Versioned};

fn versioned(value: Vec<u8>, version: i64, expires: Option<i64>) -> Versioned {
    Versioned {
        value,
        version: Version(uint(version)),
        expires_at: expires.map(from_nanos),
    }
}

fn is_live(expires: Option<i64>, now: StoreTime) -> bool {
    expires.is_none_or(|at| at > to_nanos(now))
}

pub(super) fn get(
    conn: &Connection,
    key: &str,
    now: StoreTime,
) -> Result<Option<Versioned>, StoreError> {
    let row = conn
        .query_row(
            "SELECT value, version, expires_at FROM kv WHERE key = ?1",
            [key],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                ))
            },
        )
        .optional()?;
    Ok(row
        .filter(|(_, _, expires)| is_live(*expires, now))
        .map(|(value, version, expires)| versioned(value, version, expires)))
}

pub(super) fn scan_prefix(
    conn: &Connection,
    prefix: &str,
    now: StoreTime,
) -> Result<Vec<(String, Versioned)>, StoreError> {
    let mut statement = conn
        .prepare("SELECT key, value, version, expires_at FROM kv WHERE key >= ?1 ORDER BY key")?;
    let mut rows = statement.query([prefix])?;
    let mut found = Vec::new();
    while let Some(row) = rows.next()? {
        let key: String = row.get(0)?;
        if !key.starts_with(prefix) {
            break;
        }
        let expires: Option<i64> = row.get(3)?;
        if is_live(expires, now) {
            found.push((key, versioned(row.get(1)?, row.get(2)?, expires)));
        }
    }
    Ok(found)
}

pub(super) fn changes(
    conn: &mut Connection,
    after: Revision,
    limit: usize,
) -> Result<Changes, StoreError> {
    let tx = Transaction::new(conn, TransactionBehavior::Deferred)?;
    let head: i64 = tx.query_row("SELECT value FROM meta WHERE name = 'head'", [], |row| {
        row.get(0)
    })?;
    let mut statement = tx.prepare(
        "SELECT revision, key, kind, value FROM feed WHERE revision > ?1 ORDER BY revision LIMIT ?2",
    )?;
    let page = statement.query_map(
        [
            i64::try_from(after.0).unwrap_or(i64::MAX),
            i64::try_from(limit).unwrap_or(i64::MAX),
        ],
        |row| {
            let value: Option<Vec<u8>> = row.get(3)?;
            let event = match row.get::<_, i64>(2)? {
                KIND_PUT => Event::Put(value.unwrap_or_default()),
                KIND_DELETE => Event::Delete,
                _ => Event::Expire,
            };
            Ok(Change {
                revision: Revision(uint(row.get(0)?)),
                key: row.get(1)?,
                event,
            })
        },
    )?;
    let rows = page.collect::<rusqlite::Result<Vec<Change>>>()?;
    // A full page resumes after its last row; a short one has reached the head, and every
    // revision up to it has committed (there is one writer).
    let cursor = if rows.len() >= limit {
        rows.last().map_or(after, |change| change.revision)
    } else {
        Revision(uint(head).max(after.0))
    };
    Ok(Changes { rows, cursor })
}
