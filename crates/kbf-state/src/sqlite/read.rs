//! The read side: queries on a connection of their own, which in WAL mode never wait
//! for the writer and never see a transaction that has not committed.

use std::ops::Bound;

use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior};

use super::schema::{self, KIND_DELETE, KIND_EXPIRE, KIND_PUT};
use super::writer::{from_nanos, to_nanos, to_sql, uint};
use crate::clock::StoreTime;
use crate::store::{Span, page_of};
use crate::types::{
    Change, Changes, Direction, Event, Revision, ScanPage, ScanRange, StoreError, Version,
    Versioned,
};

fn versioned(value: Vec<u8>, version: i64, expires: Option<i64>) -> Versioned {
    Versioned {
        value,
        version: Version(uint(version)),
        expires_at: expires.map(from_nanos),
    }
}

/// A `kv` row: its value, version and expiry.
fn stored(row: &Row<'_>) -> rusqlite::Result<(Vec<u8>, i64, Option<i64>)> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
}

/// A `feed` row: its revision, and the change it records, which is `None` for a kind this
/// binary does not know (a newer release wrote it). That row is skipped, not applied as
/// some other kind, and logged.
fn feed_row(row: &Row<'_>) -> rusqlite::Result<(Revision, Option<Change>)> {
    let revision = Revision(uint(row.get(0)?));
    let event = match row.get::<_, i64>(2)? {
        KIND_PUT => Event::Put(row.get::<_, Option<Vec<u8>>>(3)?.unwrap_or_default()),
        KIND_DELETE => Event::Delete,
        KIND_EXPIRE => Event::Expire,
        kind => {
            tracing::warn!("skipped feed row {revision:?} of unknown kind {kind}");
            return Ok((revision, None));
        }
    };
    let key = row.get(1)?;
    let change = Change {
        revision,
        key,
        event,
    };
    Ok((revision, Some(change)))
}

fn is_live(expires: Option<i64>, now: StoreTime) -> bool {
    match expires {
        Some(at) => at > to_nanos(now),
        None => true,
    }
}

pub(super) fn get(
    conn: &Connection,
    key: &str,
    now: StoreTime,
) -> Result<Option<Versioned>, StoreError> {
    let sql = "SELECT value, version, expires_at FROM kv WHERE key = ?1";
    let row = conn.query_row(sql, [key], stored).optional()?;
    Ok(match row {
        Some((value, version, expires)) if is_live(expires, now) => {
            Some(versioned(value, version, expires))
        }
        _ => None,
    })
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

/// C8 on one statement, so the page is read from one WAL snapshot. The statement is
/// bounded by the request's span (its bounds clipped to the prefix; below always, above
/// unless the prefix has no successor) and walks the primary key in the scan's
/// direction; expired rows are skipped in the loop, which reads up to one live row past
/// the limit to learn whether a resume token is due.
pub(super) fn scan_range(
    conn: &Connection,
    scan: ScanRange<'_>,
    now: StoreTime,
) -> Result<ScanPage, StoreError> {
    let Some(span) = Span::of(&scan) else {
        return Ok(ScanPage {
            rows: Vec::new(),
            resume: None,
        });
    };
    let lower_op = if span.lower.included { ">=" } else { ">" };
    let mut clauses = vec![format!("key {lower_op} ?1")];
    let mut params: Vec<&str> = vec![&span.lower.key];
    let upper = match &span.upper {
        Bound::Included(key) => Some(("<=", key)),
        Bound::Excluded(key) => Some(("<", key)),
        Bound::Unbounded => None,
    };
    if let Some((op, key)) = upper {
        params.push(key);
        clauses.push(format!("key {op} ?2"));
    }
    let order = match scan.direction {
        Direction::Forward => "ASC",
        Direction::Reverse => "DESC",
    };
    let sql = format!(
        "SELECT key, value, version, expires_at FROM kv WHERE {} ORDER BY key {order}",
        clauses.join(" AND ")
    );
    let take = scan.limit.get().saturating_add(1);
    let mut statement = conn.prepare(&sql)?;
    let mut rows = statement.query(rusqlite::params_from_iter(params))?;
    let mut found = Vec::new();
    while found.len() < take
        && let Some(row) = rows.next()?
    {
        let expires: Option<i64> = row.get(3)?;
        if is_live(expires, now) {
            found.push((row.get(0)?, versioned(row.get(1)?, row.get(2)?, expires)));
        }
    }
    Ok(page_of(found, scan.limit))
}

pub(super) fn changes(
    conn: &mut Connection,
    after: Revision,
    limit: usize,
) -> Result<Changes, StoreError> {
    changes_after_head(conn, after, limit, &mut nothing)
}

fn nothing() {}

/// [`changes`] with `between` called once the head has been read and before the rows
/// are, so a test can commit a group at the one moment a reader that did not hold a
/// single snapshot would see the head and the rows of different commits.
///
/// The head and the rows come from one deferred read transaction, which in WAL mode
/// pins the snapshot at its first read: the rows never run past the head, so a short page
/// can end at the head without skipping a row that committed after it was read.
///
/// A row of a kind this binary does not know is left out of the page, but it counts
/// toward the limit and the cursor: a full page resumes after its last row read, skipped
/// or not, so the poller moves on.
pub(super) fn changes_after_head(
    conn: &mut Connection,
    after: Revision,
    limit: usize,
    between: &mut dyn FnMut(),
) -> Result<Changes, StoreError> {
    let tx = Transaction::new(conn, TransactionBehavior::Deferred)?;
    let head = schema::head(&tx)?;
    between();
    let mut statement = tx.prepare(
        "SELECT revision, key, kind, value FROM feed WHERE revision > ?1 ORDER BY revision LIMIT ?2",
    )?;
    let bounds = [to_sql(after.0), to_sql(limit as u64)];
    let page = statement.query_map(bounds, feed_row)?;
    let read = page.collect::<rusqlite::Result<Vec<(Revision, Option<Change>)>>>()?;
    // A full page resumes after its last row; a short one has reached the head, and every
    // revision up to it has committed (there is one writer).
    let cursor = if read.len() >= limit {
        match read.last() {
            Some((revision, _)) => *revision,
            None => after,
        }
    } else {
        Revision(uint(head).max(after.0))
    };
    let rows = read.into_iter().filter_map(|(_, change)| change).collect();
    Ok(Changes { rows, cursor })
}
