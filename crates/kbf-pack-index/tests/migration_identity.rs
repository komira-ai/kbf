//! The identity of the pack index's migration, which no server is needed to check: the
//! database records a migration by its number, name and kind, and the file's bytes are what
//! an existing database ran. A change to any of them is a change to what a deployed
//! database has, so it is refused here, not found out on a production schema.
#![cfg(feature = "postgres")]

use kbf_pack_index::schema::{self, NAME, SQL, VERSION};
use kbf_state::postgres::schema::{self as state_schema, Kind};
use sha2::{Digest, Sha256};

/// The SHA-256 of `migrations/0002_pack_index.sql` as it was when the state store in this
/// repository shipped it (it lived under `kbf-state`'s Postgres migrations before the store moved
/// to jobscheduler's `js-state`, whose migrations end at `0001_state.sql`), written out so a
/// changed byte fails here.
const SHA256: &str = "0870c0fda5f68d6cb347bbea399f9e9b1da4d1a9179751d464a43cee6774bbc8";

/// Asserts that the migration is number 2, named `pack_index`, an expand step, and that its
/// file is byte for byte the one a database that already applied it ran. Catches: an edit
/// to the file (even whitespace), a renumbering and a rename.
#[test]
fn the_migration_is_what_an_existing_database_recorded() {
    let hex: String = Sha256::digest(SQL.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(hex, SHA256, "the migration file changed");
    let migration = schema::migration();
    assert_eq!(migration.version(), 2);
    assert_eq!(VERSION, 2);
    assert_eq!(migration.name(), "pack_index");
    assert_eq!(NAME, "pack_index");
    assert_eq!(migration.kind(), Kind::Expand);
}

/// Asserts that the set a release embeds is the state store's migration followed by this
/// one, numbered 1 and 2, and that the state store's own set stops at the first (so a caller
/// that takes its set from the state store alone would not create the pack tables, which the
/// tests of the callers pin by taking theirs from here).
#[test]
fn the_release_set_is_the_state_store_then_the_pack_index() {
    let own = state_schema::embedded();
    assert_eq!(own.len(), 1);
    let set = schema::embedded();
    let named: Vec<(u32, &str)> = set.iter().map(|m| (m.version(), m.name())).collect();
    assert_eq!(named, [(1, "state"), (2, "pack_index")]);
    assert_eq!(&set[..1], own.as_slice());
    assert_eq!(schema::latest(), 2);
    assert_eq!(state_schema::latest_of(&set), schema::latest());
    assert_eq!(state_schema::latest(), 1);
}
