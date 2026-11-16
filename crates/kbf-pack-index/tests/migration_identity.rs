//! The identity of the pack index's migration, which no server is needed to check: the
//! database records a migration by its number, name and kind, and the file's bytes are what
//! an existing database ran. A change to any of them is a change to what a deployed
//! database has, so it is refused here, not found out on a production schema.
#![cfg(feature = "postgres")]

use kbf_pack_index::schema::{
    self, ACTION_CACHE_NAME, ACTION_CACHE_SQL, ACTION_CACHE_VERSION, NAME, SQL, VERSION,
};
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

/// The SHA-256 of `migrations/0003_action_cache.sql`, written out so a changed byte fails here.
const ACTION_CACHE_SHA256: &str =
    "c742a674dee23ea4cced2e96e1f120c339591489e56b8b573dc375a40a0c8e26";

/// Asserts that the action cache's migration is number 3, named `action_cache`, an expand step,
/// and that its file is the one this test pins, which a deployed database may have run.
/// Catches: an edit to the file (even whitespace), a renumbering and a rename.
#[test]
fn the_action_cache_migration_is_pinned() {
    let hex: String = Sha256::digest(ACTION_CACHE_SQL.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(hex, ACTION_CACHE_SHA256, "the migration file changed");
    let migration = schema::action_cache_migration();
    assert_eq!(migration.version(), 3);
    assert_eq!(ACTION_CACHE_VERSION, 3);
    assert_eq!(migration.name(), "action_cache");
    assert_eq!(ACTION_CACHE_NAME, "action_cache");
    assert_eq!(migration.kind(), Kind::Expand);
}

/// Asserts that the set a release embeds is the state store's first migration followed by the
/// pack index's and the action cache's, numbered 1, 2 and 3, though the state store's own set
/// goes on past the first (its migration 2, `writer_generation`, would take the number a
/// database recorded for `pack_index`). Catches: the state store's whole set taken, which
/// would renumber or rename what a deployed database has; a caller that took its set from the
/// state store alone would not create the pack tables, which the tests of the callers pin by
/// taking theirs from here.
#[test]
fn the_release_set_is_the_state_stores_first_then_the_pack_index_then_the_action_cache() {
    let own = state_schema::embedded();
    let own_named: Vec<(u32, &str)> = own.iter().map(|m| (m.version(), m.name())).collect();
    assert_eq!(own_named, [(1, "state"), (2, "writer_generation")]);
    assert_eq!(schema::STATE_STORE_UPTO, 1);
    let set = schema::embedded();
    let named: Vec<(u32, &str)> = set.iter().map(|m| (m.version(), m.name())).collect();
    assert_eq!(
        named,
        [(1, "state"), (2, "pack_index"), (3, "action_cache")]
    );
    assert_eq!(&set[..1], &own[..1]);
    assert_eq!(schema::latest(), 3);
    assert_eq!(state_schema::latest_of(&set), schema::latest());
}
