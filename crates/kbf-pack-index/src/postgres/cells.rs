//! The cell rows and unlisted trees on Postgres: the six calls of [`crate::cells`] for
//! [`PostgresPackIndex`], in the `object_cells` and `unlisted_tree` tables of the state schema
//! (migration 5, [`crate::schema`]).
//!
//! **One statement per call.** A read is one `SELECT`; a record of cells is one `INSERT ... ON
//! CONFLICT DO NOTHING`, and only when it inserted nothing a second `SELECT` of the row that
//! is there (rows are never changed or removed, so it exists) tells the same cells from other
//! ones; a record of an unlisted tree is one upsert.
//!
//! **Outside the head lock**, like the pack index: the statements touch `object_cells`,
//! `unlisted_tree` and, for [`crate::PackIndex::live_packs`], read `pack`.
//!
//! **Order.** Trees compare as bytes (`bytea`), pack keys with the `"C"` collation, so the
//! order is the one the in-memory index has whatever the database's collation is.
//!
//! **A schema without the tables.** They are created by migration 5. A database at migration
//! 4 still opens for the pack index, and a call on them is [`PackIndexError::NotMigrated`].

use std::time::Duration;

use postgres::types::ToSql;

use super::action::failed_on_table;
use super::{PostgresPackIndex, big, failed, ordinal, unbig};
use crate::cells::{LivePack, ObjectCells, Unlisted};
use crate::{PackIndexError, check_batch, check_page, micros};

/// `$1` keys: for each key that has a row, its place in the question, size and digests.
const READ: &str = concat!(
    "SELECT r.ord, c.size, c.digests FROM UNNEST($1::text[]) WITH ORDINALITY AS r(key, ord) ",
    "JOIN object_cells c ON c.key = r.key"
);

/// `$1` key, `$2` size, `$3` digests: inserts the row when the key has none.
const INSERT: &str = concat!(
    "INSERT INTO object_cells (key, size, digests) VALUES ($1, $2, $3) ",
    "ON CONFLICT (key) DO NOTHING"
);

/// `$1` key: the row that is there.
const HELD: &str = "SELECT size, digests FROM object_cells WHERE key = $1";

/// `$1` tree, `$2` the time: a new row has the time as its first and last, a row there gets it
/// as its last.
const UPSERT: &str = concat!(
    "INSERT INTO unlisted_tree (tree, first_us, last_us) VALUES ($1, $2, $2) ",
    "ON CONFLICT (tree) DO UPDATE SET last_us = EXCLUDED.last_us"
);

/// `$1` the earliest last time, `$2` the cursor (null for none), `$3` the limit.
const UNLISTED: &str = concat!(
    "SELECT tree, first_us, last_us FROM unlisted_tree ",
    "WHERE last_us >= $1::bigint AND ($2::bytea IS NULL OR tree > $2::bytea) ",
    "ORDER BY tree LIMIT $3::bigint"
);

/// `$1` tree.
const DROP: &str = "DELETE FROM unlisted_tree WHERE tree = $1";

/// `$1` the cursor (null for none), `$2` the limit.
const LIVE: &str = concat!(
    "SELECT p.key, p.size, EXISTS (SELECT 1 FROM object_cells c WHERE c.key = p.key) ",
    "FROM pack p WHERE p.live_blobs > 0 ",
    "AND ($1::text IS NULL OR p.key COLLATE \"C\" > $1::text COLLATE \"C\") ",
    "ORDER BY p.key COLLATE \"C\" LIMIT $2::bigint"
);

/// A 32-byte value from a column that holds one.
fn digest_of(bytes: &[u8]) -> [u8; 32] {
    <[u8; 32]>::try_from(bytes).unwrap_or([0; 32])
}

/// A row's size and concatenated digests as the cells they were written from.
fn cells_of(size: i64, digests: &[u8]) -> ObjectCells {
    ObjectCells {
        size: unbig(size),
        digests: digests.chunks_exact(32).map(digest_of).collect(),
    }
}

/// A page limit as the `bigint` the statements take.
fn limit_of(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}

impl PostgresPackIndex {
    /// [`crate::PackIndex::object_cells`].
    pub(super) fn read_cells(
        &self,
        keys: &[String],
    ) -> Result<Vec<Option<ObjectCells>>, PackIndexError> {
        check_batch(keys.len())?;
        let mut session = self.session()?;
        let found = session.conn().query(READ, &[&keys]);
        let found = found.map_err(failed_on_table)?;
        let mut cells = vec![None; keys.len()];
        for row in &found {
            let digests: Vec<u8> = row.get(2);
            cells[ordinal(row.get(0))] = Some(cells_of(row.get(1), &digests));
        }
        Ok(cells)
    }

    /// [`crate::PackIndex::record_cells`].
    pub(super) fn insert_cells(
        &self,
        key: &str,
        cells: &ObjectCells,
    ) -> Result<bool, PackIndexError> {
        if key.is_empty() {
            return Err(PackIndexError::Invalid("the object key is empty"));
        }
        cells.checked()?;
        let size = big(cells.size);
        let digests: Vec<u8> = cells.digests.concat();
        let mut session = self.session()?;
        let args: [&(dyn ToSql + Sync); 3] = [&key, &size, &digests];
        let inserted = session.conn().execute(INSERT, &args);
        if inserted.map_err(failed_on_table)? > 0 {
            return Ok(true);
        }
        let held = session.conn().query_one(HELD, &[&key]).map_err(failed)?;
        let held_digests: Vec<u8> = held.get(1);
        if cells_of(held.get(0), &held_digests) == *cells {
            return Ok(false);
        }
        Err(PackIndexError::Conflict {
            key: key.to_owned(),
        })
    }

    /// [`crate::PackIndex::record_unlisted`].
    pub(super) fn upsert_unlisted(&self, tree: &[u8; 32]) -> Result<(), PackIndexError> {
        let mut session = self.session()?;
        let now = self.now(session.conn())?;
        let tree = tree.to_vec();
        let args: [&(dyn ToSql + Sync); 2] = [&tree, &now];
        let wrote = session.conn().execute(UPSERT, &args);
        wrote.map_err(failed_on_table)?;
        Ok(())
    }

    /// [`crate::PackIndex::unlisted`].
    pub(super) fn read_unlisted(
        &self,
        within: Duration,
        after: Option<&[u8; 32]>,
        limit: usize,
    ) -> Result<Vec<Unlisted>, PackIndexError> {
        check_page(limit)?;
        let mut session = self.session()?;
        let since = self.now(session.conn())?.saturating_sub(micros(within));
        let after: Option<Vec<u8>> = after.map(|tree| tree.to_vec());
        let limit = limit_of(limit);
        let args: [&(dyn ToSql + Sync); 3] = [&since, &after, &limit];
        let found = session.conn().query(UNLISTED, &args);
        let found = found.map_err(failed_on_table)?;
        Ok(found
            .iter()
            .map(|row| Unlisted {
                tree: digest_of(row.get::<_, &[u8]>(0)),
                first_us: row.get(1),
                last_us: row.get(2),
            })
            .collect())
    }

    /// [`crate::PackIndex::drop_unlisted`].
    pub(super) fn delete_unlisted(&self, tree: &[u8; 32]) -> Result<bool, PackIndexError> {
        let mut session = self.session()?;
        let tree = tree.to_vec();
        let removed = session.conn().execute(DROP, &[&tree]);
        Ok(removed.map_err(failed_on_table)? > 0)
    }

    /// [`crate::PackIndex::live_packs`].
    pub(super) fn read_live_packs(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LivePack>, PackIndexError> {
        check_page(limit)?;
        let mut session = self.session()?;
        let limit = limit_of(limit);
        let found = session.conn().query(LIVE, &[&after, &limit]);
        let found = found.map_err(failed_on_table)?;
        Ok(found
            .iter()
            .map(|row| LivePack {
                key: row.get(0),
                size: unbig(row.get(1)),
                has_cells: row.get(2),
            })
            .collect())
    }
}
