//! The Directory rows on Postgres: [`crate::PackIndex::directories`] and
//! [`crate::PackIndex::record_directories`] for [`PostgresPackIndex`], in the `directory`
//! table of the state schema (migration 4, [`crate::schema`]).
//!
//! **One statement per call.** A read is one `SELECT` over the digests unnested; a record is
//! one `INSERT ... SELECT` of the checked Directories in digest order, `ON CONFLICT DO
//! NOTHING`, so it is atomic on its own and two records that name the same digests take
//! their rows in the same order. A digest that has a row keeps it: the row is the same bytes.
//!
//! **Outside the head lock**, like the pack index: the statements touch `directory` only.
//!
//! **A schema without the table.** The table is created by migration 4. A database that is at
//! migration 3 still opens for the pack index, and a call on the Directory rows is
//! [`PackIndexError::NotMigrated`].

use postgres::types::ToSql;

use super::action::failed_on_table;
use super::{PostgresPackIndex, big, ordinal, unnest};
use crate::directory::{NewDirectory, checked};
use crate::{BlobDigest, PackIndexError, check_batch};

/// `$1` hashes, `$2` sizes: for each digest that has a row, its place in the question and its
/// body.
const READ: &str = concat!(
    "SELECT r.ord, d.body FROM UNNEST($1::bytea[], $2::bigint[]) ",
    "WITH ORDINALITY AS r(hash, size, ord) ",
    "JOIN directory d ON d.hash = r.hash AND d.size = r.size"
);

/// `$1` hashes, `$2` sizes, `$3` bodies, `$4` the time: inserts the Directories that have no
/// row, in the order given (digest order), and says how many it inserted.
const INSERT: &str = concat!(
    "INSERT INTO directory (hash, size, body, written_us) ",
    "SELECT r.hash, r.size, r.body, $4::bigint ",
    "FROM UNNEST($1::bytea[], $2::bigint[], $3::bytea[]) ",
    "WITH ORDINALITY AS r(hash, size, body, ord) ORDER BY r.ord ",
    "ON CONFLICT (hash, size) DO NOTHING"
);

impl PostgresPackIndex {
    /// [`crate::PackIndex::directories`].
    pub(super) fn read_directories(
        &self,
        digests: &[BlobDigest],
    ) -> Result<Vec<Option<Vec<u8>>>, PackIndexError> {
        check_batch(digests.len())?;
        let (hashes, sizes) = unnest(digests);
        let mut session = self.session()?;
        let found = session.conn().query(READ, &[&hashes, &sizes]);
        let found = found.map_err(failed_on_table)?;
        let mut bodies = vec![None; digests.len()];
        for row in &found {
            bodies[ordinal(row.get(0))] = Some(row.get(1));
        }
        Ok(bodies)
    }

    /// [`crate::PackIndex::record_directories`].
    pub(super) fn insert_directories(
        &self,
        directories: &[NewDirectory],
    ) -> Result<usize, PackIndexError> {
        let sorted = checked(directories)?;
        if sorted.is_empty() {
            return Ok(0);
        }
        let hashes: Vec<Vec<u8>> = sorted.iter().map(|d| d.digest.hash.to_vec()).collect();
        let sizes: Vec<i64> = sorted.iter().map(|d| big(d.digest.size)).collect();
        let bodies: Vec<&[u8]> = sorted.iter().map(|d| d.body.as_slice()).collect();
        let mut session = self.session()?;
        let now = self.now(session.conn())?;
        let args: [&(dyn ToSql + Sync); 4] = [&hashes, &sizes, &bodies, &now];
        let inserted = session.conn().execute(INSERT, &args);
        let inserted = inserted.map_err(failed_on_table)?;
        Ok(usize::try_from(inserted).unwrap_or(usize::MAX))
    }
}
