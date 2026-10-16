//! Red-first stub: the layout lands with the implementation.

use rusqlite::Connection;

use super::SqliteError;

pub(super) fn configure(_conn: &Connection) -> Result<(), SqliteError> {
    todo!("av2-03: configure")
}
