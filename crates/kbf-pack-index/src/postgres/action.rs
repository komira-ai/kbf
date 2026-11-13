//! The action cache on Postgres: [`ActionStore`] for [`PostgresPackIndex`], in the
//! `action_result` table of the state schema (migration 3, [`crate::schema`]).
//!
//! **One transaction per put.** The packs of the call are recorded as `record_pack` records
//! them, in the order of their keys, and the action's row is written last, in the same
//! transaction at read committed; the transaction either commits all of it or none. Two puts
//! that name the same packs take their rows in the same order, and every put takes its action
//! row after its pack rows, so they never wait on each other in a cycle.
//!
//! **Outside the head lock**, like the pack index: the statements touch `pack`, `blob` and
//! `action_result` only.
//!
//! **A schema without the table.** The action table is created by migration 3. A database that
//! is at migration 2 still opens for the pack index (which does not need the table), and a
//! call of the action store on it is [`PackIndexError::NotMigrated`]; the put rolls back, so
//! its packs are not recorded either.

use postgres::error::SqlState;
use postgres::types::ToSql;

use super::{PostgresPackIndex, Rows, big, failed};
use crate::action::{ActionEntry, ActionPut, ActionStore, check_result, in_key_order};
use crate::{BlobDigest, NewPack, PackIndexError, Recorded};

/// `$1` hash, `$2` size: the row's result and the time it was written.
const GET_ACTION: &str =
    "SELECT result, written_us FROM action_result WHERE hash = $1 AND size = $2";

/// `$1` hash, `$2` size, `$3` result, `$4` the time: writes the row, replacing one, and says
/// whether it replaced one (`xmax` is set on a row an update has written, zero on an insert).
const PUT_ACTION: &str = concat!(
    "INSERT INTO action_result (hash, size, result, written_us) VALUES ($1, $2, $3, $4) ",
    "ON CONFLICT (hash, size) DO UPDATE ",
    "SET result = EXCLUDED.result, written_us = EXCLUDED.written_us ",
    "RETURNING (xmax <> 0)"
);

/// A failed statement on the action table: a table that is not there is a schema that is not
/// migrated, anything else is the store failing.
fn failed_on_table(err: postgres::Error) -> PackIndexError {
    if err.code() == Some(&SqlState::UNDEFINED_TABLE) {
        PackIndexError::NotMigrated
    } else {
        failed(err)
    }
}

impl PostgresPackIndex {
    /// `put_action` inside its transaction.
    fn put_in(
        client: &mut super::Conn,
        action: &BlobDigest,
        result: &[u8],
        packs: &[(usize, &NewPack, Rows)],
        total: usize,
        now: i64,
    ) -> Result<ActionPut, PackIndexError> {
        let mut recorded: Vec<Option<Recorded>> = vec![None; total];
        for (place, pack, rows) in packs {
            recorded[*place] = Some(Self::record_in(client, pack, rows, now)?);
        }
        let (hash, size) = (action.hash.to_vec(), big(action.size));
        let args: [&(dyn ToSql + Sync); 4] = [&hash, &size, &result, &now];
        let wrote = client
            .query_one(PUT_ACTION, &args)
            .map_err(failed_on_table)?;
        Ok(ActionPut {
            replaced: wrote.get(0),
            packs: recorded.into_iter().flatten().collect(),
        })
    }
}

impl ActionStore for PostgresPackIndex {
    fn get_action(&self, action: &BlobDigest) -> Result<Option<ActionEntry>, PackIndexError> {
        let (hash, size) = (action.hash.to_vec(), big(action.size));
        let mut session = self.session()?;
        let found = session.conn().query_opt(GET_ACTION, &[&hash, &size]);
        let found = found.map_err(failed_on_table)?;
        Ok(found.map(|row| ActionEntry {
            result: row.get(0),
            written_us: row.get(1),
        }))
    }

    fn put_action(
        &self,
        action: &BlobDigest,
        result: &[u8],
        packs: &[NewPack],
    ) -> Result<ActionPut, PackIndexError> {
        check_result(result)?;
        let mut checked = Vec::with_capacity(packs.len());
        for (place, pack) in in_key_order(packs) {
            let rows = Rows::of(&pack.checked()?);
            checked.push((place, pack, rows));
        }
        let mut session = self.session()?;
        let now = self.now(session.conn())?;
        let client = session.conn();
        client.batch_execute("BEGIN").map_err(failed)?;
        let put = Self::put_in(client, action, result, &checked, packs.len(), now);
        let end = match &put {
            Ok(_) => "COMMIT",
            Err(_) => "ROLLBACK",
        };
        let ended = client.batch_execute(end);
        if ended.is_err() {
            session.discard();
        }
        let put = put?;
        ended.map_err(failed)?;
        Ok(put)
    }
}
