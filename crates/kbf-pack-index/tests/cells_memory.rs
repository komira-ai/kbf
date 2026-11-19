//! The in-memory index against the cell rows' and unlisted trees' conformance clauses (the
//! clauses `tests/postgres_cells.rs` runs against Postgres), and the answers of an index that
//! keeps neither.

use kbf_pack_index::PackIndexError;
use kbf_pack_index::cells::ObjectCells;
use kbf_pack_index::cells_conformance::run_all;
use kbf_pack_index::memory::MemoryPackBackend;

/// Asserts that the in-memory index passes every clause of the suite: cells read back in the
/// order asked, the same cells twice, a conflict that changes nothing, cells that cannot exist,
/// the size boundaries, the batch bound, the empty key, unlisted trees recorded, moved, paged,
/// windowed and dropped, and the live-pack sweep.
#[test]
fn the_memory_index_passes_every_cells_clause() {
    run_all(&MemoryPackBackend);
}

/// An index that keeps only packs, as one reached through a service that takes no cell calls: it
/// implements the required calls and leaves the cell calls to their defaults.
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

/// Asserts that an index that does not keep cell rows answers every one of the six cell calls
/// `Unsupported`.
#[test]
fn an_index_without_cell_rows_answers_unsupported() {
    use kbf_pack_index::PackIndex as _;
    let unsupported = PackIndexError::Unsupported;
    let cells = ObjectCells {
        size: 1,
        digests: vec![[1; 32]],
    };
    assert_eq!(PacksOnly.object_cells(&[]), Err(unsupported.clone()));
    assert_eq!(
        PacksOnly.record_cells("k", &cells),
        Err(unsupported.clone())
    );
    assert_eq!(
        PacksOnly.record_unlisted(&[1; 32]),
        Err(unsupported.clone())
    );
    assert_eq!(
        PacksOnly.unlisted(std::time::Duration::ZERO, None, 1),
        Err(unsupported.clone())
    );
    assert_eq!(PacksOnly.drop_unlisted(&[1; 32]), Err(unsupported.clone()));
    assert_eq!(PacksOnly.live_packs(None, 1), Err(unsupported));
}
