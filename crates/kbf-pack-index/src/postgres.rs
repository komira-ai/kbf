//! The pack index on Postgres: [`PostgresPackIndex`] keeps [`PackIndex`] in the `pack` and
//! `blob` tables of the state schema (migration 2, [`crate::schema`]).
//!
//! **Outside the head lock.** The index has connections of its own and its statements touch
//! only `pack` and `blob`. A state transaction takes the commit head (`LOCK TABLE head` and
//! the head row) before anything else; recording a pack takes neither, so it runs while a
//! state transaction holds the head, and the other way round.
//!
//! **One transaction per pack.** `record_pack` inserts the pack row and its blob rows, and
//! sets the pack's row count, in one transaction at read committed. The pack's key is
//! unique, so a key recorded twice meets the first row; the blob rows go in with `ON CONFLICT
//! DO NOTHING`, so a blob that has a row keeps it. Blob rows go in sorted by digest, so two
//! transactions that name the same blobs queue on them in the same order and never wait on
//! each other in a cycle.
//!
//! **Choices the table layout leaves open.** `blob` has a plain index on `pack_id`: the
//! question "does any blob point at this pack" and the blobs of a pack are asked of it, and
//! it is not on `last_touch_us`, so a touch stays a heap-only update on the `blob` page
//! (the table's fillfactor leaves the room). A pack is dead when no `blob` row points at it,
//! read from the rows and not from `live_blobs`, which is the count the record left.
//!
//! Without the flag nothing opens this index and no row of either table is read or written.

use std::sync::Arc;
use std::time::Duration;

use postgres::types::ToSql;

use kbf_state::Clock;
use kbf_state::postgres::{Conn, ExtensionPool, Loan, PostgresConfig, store_error};

use crate::{
    BlobDigest, Location, NewPack, PackBlob, PackIndex, PackIndexError, PackInfo, Recorded,
    check_batch, count, micros, micros_of,
};

#[cfg(any(test, feature = "test-support"))]
mod backend;
#[cfg(any(test, feature = "test-support"))]
pub use backend::PostgresPackBackend;

/// Whether both pack tables are in the schema the session searches.
const TABLES: &str = "SELECT to_regclass('pack') IS NOT NULL AND to_regclass('blob') IS NOT NULL";

/// The database's clock in microseconds, as the state store reads it.
const NOW: &str = "SELECT floor(extract(epoch from clock_timestamp()) * 1000000)::bigint";

/// `$1` key, `$2` writer, `$3` size, `$4` the time, `$5` blob count: the new pack's id, or
/// no row when the key is recorded already.
const INSERT_PACK: &str = concat!(
    "INSERT INTO pack (key, writer, size, created_us, blob_count, live_blobs) ",
    "VALUES ($1, $2, $3, $4, $5, 0) ON CONFLICT (key) DO NOTHING RETURNING pack_id"
);

/// `$1` hashes, `$2` sizes, `$3` offsets, `$4` the pack's id, `$5` the time: the blobs that
/// had no row get one, in digest order.
const INSERT_BLOBS: &str = concat!(
    "INSERT INTO blob (hash, size, pack_id, off, last_touch_us) ",
    "SELECT r.hash, r.size, $4::bigint, r.off, $5::bigint ",
    "FROM UNNEST($1::bytea[], $2::bigint[], $3::bigint[]) AS r(hash, size, off) ",
    "ORDER BY r.hash, r.size ON CONFLICT (hash, size) DO NOTHING"
);

/// `$1` pack id, `$2` the number of rows the record made.
const SET_LIVE: &str = "UPDATE pack SET live_blobs = $2 WHERE pack_id = $1";

/// `$1` key: the pack's id and row.
const FIND_PACK: &str = concat!(
    "SELECT pack_id, key, writer, size, created_us, blob_count, live_blobs ",
    "FROM pack WHERE key = $1"
);

/// `$1` hashes, `$2` sizes, `$3` offsets, `$4` a pack's id: how many of the blobs have a row
/// in that pack at that offset.
const MATCHED: &str = concat!(
    "SELECT count(*) FROM UNNEST($1::bytea[], $2::bigint[], $3::bigint[]) ",
    "AS r(hash, size, off) JOIN blob b ON b.hash = r.hash AND b.size = r.size ",
    "WHERE b.pack_id = $4::bigint AND b.off = r.off"
);

/// `$1` hashes, `$2` sizes: for each digest that has a row, its place in the question, its
/// pack's key, offset, length and touch time.
const LOCATE: &str = concat!(
    "SELECT r.ord, p.key, b.off, b.size, b.last_touch_us ",
    "FROM UNNEST($1::bytea[], $2::bigint[]) ",
    "WITH ORDINALITY AS r(hash, size, ord) ",
    "JOIN blob b ON b.hash = r.hash AND b.size = r.size ",
    "JOIN pack p ON p.pack_id = b.pack_id"
);

/// `$1` hashes, `$2` sizes: the place in the question of each digest without a row.
const MISSING: &str = concat!(
    "SELECT r.ord FROM UNNEST($1::bytea[], $2::bigint[]) ",
    "WITH ORDINALITY AS r(hash, size, ord) ",
    "WHERE NOT EXISTS (SELECT 1 FROM blob b WHERE b.hash = r.hash AND b.size = r.size) ",
    "ORDER BY r.ord"
);

/// `$1` hashes, `$2` sizes, `$3` the time: sets the touch time of the blobs with a row. The
/// rows are locked first, in key order, so two touches that name the same blobs queue on
/// them in the same order and never wait on each other in a cycle.
const TOUCH: &str = concat!(
    "WITH t AS (SELECT b.hash, b.size FROM blob b ",
    "JOIN UNNEST($1::bytea[], $2::bigint[]) AS r(hash, size) ",
    "ON b.hash = r.hash AND b.size = r.size ",
    "ORDER BY b.hash, b.size FOR NO KEY UPDATE OF b) ",
    "UPDATE blob SET last_touch_us = $3::bigint FROM t ",
    "WHERE blob.hash = t.hash AND blob.size = t.size"
);

/// `$1` the time, `$2` the grace, `$3` the limit: the packs older than the grace that no
/// blob row points at, oldest first.
const DEAD: &str = concat!(
    "SELECT key, writer, size, created_us, blob_count, live_blobs FROM pack p ",
    "WHERE p.created_us < $1::bigint - $2::bigint ",
    "AND NOT EXISTS (SELECT 1 FROM blob b WHERE b.pack_id = p.pack_id) ",
    "ORDER BY p.pack_id LIMIT $3::bigint"
);

/// `$1` keys: the place in the list of each key no pack is recorded under.
const UNREFERENCED: &str = concat!(
    "SELECT k.key FROM UNNEST($1::text[]) WITH ORDINALITY AS k(key, ord) ",
    "WHERE NOT EXISTS (SELECT 1 FROM pack p WHERE p.key = k.key) ORDER BY k.ord"
);

/// A failed client call as the index reports it.
fn failed(err: postgres::Error) -> PackIndexError {
    PackIndexError::Store(store_error(err))
}

/// A value as the `bigint` the tables hold, saturating.
fn big(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// A `bigint` the tables hold as the unsigned value it was written from.
fn unbig(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

/// A count as the `integer` the tables hold, saturating.
fn int(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// An `integer` the tables hold as the count it was written from.
fn unint(n: i32) -> u32 {
    u32::try_from(n).unwrap_or(0)
}

/// The place of a row in a question, from the 1-based ordinal the statement numbers it by.
fn ordinal(n: i64) -> usize {
    usize::try_from(n - 1).unwrap_or(0)
}

/// The hashes and sizes of `digests` as the two arrays the statements unnest.
fn unnest(digests: &[BlobDigest]) -> (Vec<Vec<u8>>, Vec<i64>) {
    let hashes = digests.iter().map(|d| d.hash.to_vec()).collect();
    let sizes = digests.iter().map(|d| big(d.size)).collect();
    (hashes, sizes)
}

/// The blobs of a pack as the three arrays `INSERT_BLOBS` and `MATCHED` unnest.
struct Rows {
    hashes: Vec<Vec<u8>>,
    sizes: Vec<i64>,
    offsets: Vec<i64>,
}

impl Rows {
    fn of(blobs: &[PackBlob]) -> Rows {
        Rows {
            hashes: blobs.iter().map(|b| b.digest.hash.to_vec()).collect(),
            sizes: blobs.iter().map(|b| big(b.digest.size)).collect(),
            offsets: blobs.iter().map(|b| big(b.offset)).collect(),
        }
    }
}

/// A pack row from `first`: key, writer, size, creation time, blob count, live count.
fn pack_info(row: &postgres::Row, first: usize) -> PackInfo {
    PackInfo {
        key: row.get(first),
        writer: row.get(first + 1),
        size: unbig(row.get(first + 2)),
        created_us: row.get(first + 3),
        blob_count: unint(row.get(first + 4)),
        live_blobs: unint(row.get(first + 5)),
    }
}

/// The pack index in the tables of a Postgres state store.
pub struct PostgresPackIndex {
    pool: ExtensionPool,
    config: PostgresConfig,
    clock: Option<Arc<dyn Clock>>,
}

impl PostgresPackIndex {
    /// Connects, with connections of its own (at most `config`'s limit), and checks that the
    /// server flushes commits, that the state schema is one this build supports and that it
    /// has the pack tables. It creates nothing: `kbf-installer` owns the schema.
    ///
    /// # Errors
    /// [`PackIndexError::Store`] when the server cannot be reached or refuses the schema;
    /// [`PackIndexError::NotMigrated`] when the schema has no pack tables.
    pub fn open(config: &PostgresConfig) -> Result<PostgresPackIndex, PackIndexError> {
        let pool = ExtensionPool::open(config)?;
        let index = PostgresPackIndex {
            pool,
            config: config.clone(),
            clock: None,
        };
        let mut session = index.session()?;
        let row = session.conn().query_one(TABLES, &[]).map_err(failed)?;
        let found: bool = row.get(0);
        drop(session);
        if found {
            Ok(index)
        } else {
            Err(PackIndexError::NotMigrated)
        }
    }

    /// Judges time by `clock` instead of the database's. For tests.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> PostgresPackIndex {
        self.clock = Some(clock);
        self
    }

    /// Another index on the same database with the same clock and connections of its own.
    ///
    /// # Errors
    /// As [`PostgresPackIndex::open`].
    pub fn reconnect(&self) -> Result<PostgresPackIndex, PackIndexError> {
        let mut again = PostgresPackIndex::open(&self.config)?;
        again.clock = self.clock.clone();
        Ok(again)
    }

    fn session(&self) -> Result<Loan<'_>, PackIndexError> {
        Ok(self.pool.checkout()?)
    }

    /// The index's time in microseconds: the injected clock's, else the database's.
    fn now(&self, client: &mut Conn) -> Result<i64, PackIndexError> {
        let injected = self.clock.as_ref().map(|clock| micros_of(clock.now()));
        if let Some(at) = injected {
            return Ok(at);
        }
        let row = client.query_one(NOW, &[]).map_err(failed)?;
        Ok(row.get(0))
    }

    /// `record_pack` inside its transaction.
    fn record_in(
        client: &mut Conn,
        pack: &NewPack,
        rows: &Rows,
        now: i64,
    ) -> Result<Recorded, PackIndexError> {
        let size = big(pack.size);
        let blob_count = int(rows.hashes.len());
        let args: [&(dyn ToSql + Sync); 5] = [&pack.key, &pack.writer, &size, &now, &blob_count];
        let made = client.query_opt(INSERT_PACK, &args).map_err(failed)?;
        let Some(made) = made else {
            return Self::recorded_before(client, pack, rows);
        };
        let pack_id: i64 = made.get(0);
        let args: [&(dyn ToSql + Sync); 5] =
            [&rows.hashes, &rows.sizes, &rows.offsets, &pack_id, &now];
        let indexed = client.execute(INSERT_BLOBS, &args).map_err(failed)?;
        let indexed = u32::try_from(indexed).unwrap_or(u32::MAX);
        let live = i32::try_from(indexed).unwrap_or(i32::MAX);
        let set = client.execute(SET_LIVE, &[&pack_id, &live]);
        set.map_err(failed)?;
        Ok(Recorded {
            created: true,
            indexed,
        })
    }

    /// The answer for a key that has a pack: the same contents, or a conflict. The pack has
    /// the contents when its writer, size and blob count are the record's and the rows that
    /// point at it are as many as the pack counted and are the record's blobs at the
    /// record's offsets.
    fn recorded_before(
        client: &mut Conn,
        pack: &NewPack,
        rows: &Rows,
    ) -> Result<Recorded, PackIndexError> {
        let held = client.query_one(FIND_PACK, &[&pack.key]);
        let held = held.map_err(failed)?;
        let pack_id: i64 = held.get(0);
        let info = pack_info(&held, 1);
        let args: [&(dyn ToSql + Sync); 4] = [&rows.hashes, &rows.sizes, &rows.offsets, &pack_id];
        let matched = client.query_one(MATCHED, &args).map_err(failed)?;
        let matched: i64 = matched.get(0);
        let matched = u32::try_from(matched).unwrap_or(0);
        let blob_count = count(rows.hashes.len());
        let declared = (
            info.writer.as_str(),
            info.size,
            info.blob_count,
            info.live_blobs,
        );
        let offered = (pack.writer.as_str(), pack.size, blob_count, matched);
        if declared != offered {
            return Err(PackIndexError::Conflict {
                key: pack.key.clone(),
            });
        }
        Ok(Recorded {
            created: false,
            indexed: info.live_blobs,
        })
    }
}

impl PackIndex for PostgresPackIndex {
    fn record_pack(&self, pack: &NewPack) -> Result<Recorded, PackIndexError> {
        let blobs = pack.checked()?;
        let rows = Rows::of(&blobs);
        let mut session = self.session()?;
        let now = self.now(session.conn())?;
        let client = session.conn();
        client.batch_execute("BEGIN").map_err(failed)?;
        let recorded = Self::record_in(client, pack, &rows, now);
        let end = match &recorded {
            Ok(_) => "COMMIT",
            Err(_) => "ROLLBACK",
        };
        let ended = client.batch_execute(end);
        if ended.is_err() {
            session.discard();
        }
        let recorded = recorded?;
        ended.map_err(failed)?;
        Ok(recorded)
    }

    fn pack(&self, key: &str) -> Result<Option<PackInfo>, PackIndexError> {
        let mut session = self.session()?;
        let found = session.conn().query_opt(FIND_PACK, &[&key]);
        let found = found.map_err(failed)?;
        Ok(found.map(|row| pack_info(&row, 1)))
    }

    fn locate(&self, digests: &[BlobDigest]) -> Result<Vec<Option<Location>>, PackIndexError> {
        check_batch(digests.len())?;
        let (hashes, sizes) = unnest(digests);
        let mut session = self.session()?;
        let found = session.conn().query(LOCATE, &[&hashes, &sizes]);
        let found = found.map_err(failed)?;
        let mut located = vec![None; digests.len()];
        for row in &found {
            located[ordinal(row.get(0))] = Some(Location {
                pack_key: row.get(1),
                offset: unbig(row.get(2)),
                length: unbig(row.get(3)),
                last_touch_us: row.get(4),
            });
        }
        Ok(located)
    }

    fn find_missing(&self, digests: &[BlobDigest]) -> Result<Vec<BlobDigest>, PackIndexError> {
        check_batch(digests.len())?;
        let (hashes, sizes) = unnest(digests);
        let mut session = self.session()?;
        let found = session.conn().query(MISSING, &[&hashes, &sizes]);
        let found = found.map_err(failed)?;
        let missing = found.iter().map(|row| digests[ordinal(row.get(0))]);
        Ok(missing.collect())
    }

    fn touch(&self, digests: &[BlobDigest]) -> Result<usize, PackIndexError> {
        check_batch(digests.len())?;
        // Each key goes to the statement once; it takes the row locks in key order itself.
        let mut sorted = digests.to_vec();
        sorted.sort();
        sorted.dedup();
        let (hashes, sizes) = unnest(&sorted);
        let mut session = self.session()?;
        let now = self.now(session.conn())?;
        let args: [&(dyn ToSql + Sync); 3] = [&hashes, &sizes, &now];
        let touched = session.conn().execute(TOUCH, &args).map_err(failed)?;
        Ok(usize::try_from(touched).unwrap_or(usize::MAX))
    }

    fn dead_packs(&self, grace: Duration, limit: usize) -> Result<Vec<PackInfo>, PackIndexError> {
        let mut session = self.session()?;
        let now = self.now(session.conn())?;
        let grace = micros(grace);
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let args: [&(dyn ToSql + Sync); 3] = [&now, &grace, &limit];
        let found = session.conn().query(DEAD, &args).map_err(failed)?;
        Ok(found.iter().map(|row| pack_info(row, 0)).collect())
    }

    fn unreferenced_keys(&self, keys: &[String]) -> Result<Vec<String>, PackIndexError> {
        check_batch(keys.len())?;
        let mut session = self.session()?;
        let found = session.conn().query(UNREFERENCED, &[&keys]);
        let found = found.map_err(failed)?;
        Ok(found.iter().map(|row| row.get(0)).collect())
    }
}
