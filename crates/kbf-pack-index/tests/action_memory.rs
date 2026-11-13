//! The in-memory index against the action cache's conformance clauses A1 to A9 (the clauses
//! `tests/postgres_action.rs` runs against Postgres).

use kbf_pack_index::action_conformance::run_all;
use kbf_pack_index::memory::MemoryPackBackend;

/// Asserts that the in-memory index passes every clause of the action cache's suite: a result
/// read as put, a put that replaces, an empty result, a long result refused, a put that records
/// its packs, a refused pack that writes nothing, a repeated put, concurrent puts and a second
/// handle on the data.
#[test]
fn the_memory_index_passes_every_action_clause() {
    run_all(&MemoryPackBackend);
}
