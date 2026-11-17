//! The in-memory index against the Directory rows' conformance clauses D1 to D7 (the clauses
//! `tests/postgres_directory.rs` runs against Postgres).

use kbf_pack_index::PackIndexError;
use kbf_pack_index::directory_conformance::{directory, run_all};
use kbf_pack_index::memory::MemoryPackBackend;

/// Asserts that the in-memory index passes every clause of the Directory rows' suite: a row
/// read back in the order asked, a repeated record that inserts nothing, bytes that are not the
/// digest refused with nothing written, the body and batch bounds, an empty call, concurrent
/// records from two handles, and Directory rows kept apart from blob rows.
#[test]
fn the_memory_index_passes_every_directory_clause() {
    run_all(&MemoryPackBackend);
}

/// An index that keeps only packs, as one reached through a service that takes no Directory
/// calls: it implements the required calls and leaves the Directory calls to their defaults.
struct PacksOnly;

impl kbf_pack_index::PackIndex for PacksOnly {
    fn record_pack(
        &self,
        _: &kbf_pack_index::NewPack,
    ) -> Result<kbf_pack_index::Recorded, PackIndexError> {
        Err(PackIndexError::Unsupported)
    }

    fn pack(&self, _: &str) -> Result<Option<kbf_pack_index::PackInfo>, PackIndexError> {
        Ok(None)
    }

    fn locate(
        &self,
        digests: &[kbf_pack_index::BlobDigest],
    ) -> Result<Vec<Option<kbf_pack_index::Location>>, PackIndexError> {
        Ok(vec![None; digests.len()])
    }

    fn find_missing(
        &self,
        digests: &[kbf_pack_index::BlobDigest],
    ) -> Result<Vec<kbf_pack_index::BlobDigest>, PackIndexError> {
        Ok(digests.to_vec())
    }

    fn touch(&self, _: &[kbf_pack_index::BlobDigest]) -> Result<usize, PackIndexError> {
        Ok(0)
    }

    fn dead_packs(
        &self,
        _: std::time::Duration,
        _: usize,
    ) -> Result<Vec<kbf_pack_index::PackInfo>, PackIndexError> {
        Ok(Vec::new())
    }

    fn unreferenced_keys(&self, keys: &[String]) -> Result<Vec<String>, PackIndexError> {
        Ok(keys.to_vec())
    }
}

/// Asserts that an index that does not keep Directory rows answers both Directory calls
/// `Unsupported` (the walk then reads every Directory from its blob), and writes nothing.
#[test]
fn an_index_without_directory_rows_answers_unsupported() {
    use kbf_pack_index::PackIndex as _;
    let one = directory(b"dir-one");
    assert_eq!(
        PacksOnly.directories(&[one.digest]),
        Err(PackIndexError::Unsupported)
    );
    assert_eq!(
        PacksOnly.record_directories(&[one]),
        Err(PackIndexError::Unsupported)
    );
}
