//! The cells of the base tree's objects, recorded as the frontend publishes it: every tree list
//! names the whole base tree, so a list's L2 sidecar is written at its write only once they are.

use std::time::Duration;

use kbf_front::action_tree::Sidecars;
use kbf_pack_index::cells::ObjectCells;
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_pack_index::{
    BlobDigest, Location, NewPack, PackIndex, PackIndexError, PackInfo, Recorded,
};
use kbf_state::ManualClock;

use super::*;

/// A pack index in memory, and sidecars over it and `store`.
fn sidecars_over(store: &Arc<Bucket>) -> (Arc<MemoryPackIndex>, Sidecars) {
    let index = Arc::new(MemoryPackIndex::new(Arc::new(ManualClock::default())));
    let sidecars = Sidecars::new(index.clone(), store.clone());
    (index, sidecars)
}

/// The cells of `bytes`, by tierstore's own hasher.
fn cells_of(bytes: &[u8]) -> ObjectCells {
    let cells = tierstore_list::PackCells::of(bytes);
    ObjectCells {
        size: cells.size,
        digests: cells.digests,
    }
}

/// Asserts that a publish given sidecars records the cells of every object of the bundle under
/// its key `cas/l/<sha256>`, as tierstore hashes its bytes, and that a second publish of the same
/// release records none: each is recorded already. Catches: the base tree's objects left with no
/// cells, which leaves every tree list over the base with no sidecar at its write, cells recorded
/// under another key than the one a list names, and cells recorded again on every start.
#[tokio::test]
async fn a_publish_given_sidecars_records_the_cells_of_every_object_once() {
    let dir = scratch("base-publish-cells");
    let release = standard(b"kbf-task of the cells");
    install(&dir, &release);
    let store = Arc::new(Bucket::default());
    let (index, sidecars) = sidecars_over(&store);

    let published = publish(
        &dir,
        COMMIT,
        PLATFORM,
        store.clone(),
        true,
        &retry(),
        Some(&sidecars),
    )
    .await
    .expect("published");

    assert_eq!(published.cells_recorded, release.objects.len());
    let keys: Vec<String> = release.objects.iter().map(|(hex, _)| key(hex)).collect();
    let expected: Vec<Option<ObjectCells>> = (release.objects.iter())
        .map(|(_, bytes)| Some(cells_of(bytes)))
        .collect();
    assert_eq!(index.object_cells(&keys).expect("cells"), expected);

    let again = publish(
        &dir,
        COMMIT,
        PLATFORM,
        store.clone(),
        true,
        &retry(),
        Some(&sidecars),
    )
    .await
    .expect("published again");

    assert_eq!(again.cells_recorded, 0, "every object has its cells");
    assert_eq!(index.object_cells(&keys).expect("cells"), expected);
    let mut text = String::new();
    sidecars.counters().render(&mut text);
    let line = format!("kbf_l2_cells_recorded_total {}\n", release.objects.len());
    assert!(text.contains(&line), "{text}");
}

/// Asserts that a publish given no sidecars records nothing, as before: the base is published
/// and the index is not asked. Catches: cells hashed or recorded for a frontend that does not
/// list its base.
#[tokio::test]
async fn a_publish_given_no_sidecars_records_no_cells() {
    let dir = scratch("base-publish-no-cells");
    let release = standard(b"kbf-task of no cells");
    install(&dir, &release);
    let store = Arc::new(Bucket::default());

    let published = run(&dir, &store).await.expect("published");

    assert_eq!(published.cells_recorded, 0);
    assert_eq!(published.written, release.objects.len() as u64);
}

/// A pack index that keeps no cells: every call about cells is `Unsupported`.
struct NoCells;

impl PackIndex for NoCells {
    fn record_pack(&self, _: &NewPack) -> Result<Recorded, PackIndexError> {
        Err(PackIndexError::Unsupported)
    }
    fn pack(&self, _: &str) -> Result<Option<PackInfo>, PackIndexError> {
        Ok(None)
    }
    fn locate(&self, digests: &[BlobDigest]) -> Result<Vec<Option<Location>>, PackIndexError> {
        Ok(vec![None; digests.len()])
    }
    fn find_missing(&self, digests: &[BlobDigest]) -> Result<Vec<BlobDigest>, PackIndexError> {
        Ok(digests.to_vec())
    }
    fn touch(&self, _: &[BlobDigest]) -> Result<usize, PackIndexError> {
        Ok(0)
    }
    fn dead_packs(&self, _: Duration, _: usize) -> Result<Vec<PackInfo>, PackIndexError> {
        Ok(Vec::new())
    }
    fn unreferenced_keys(&self, keys: &[String]) -> Result<Vec<String>, PackIndexError> {
        Ok(keys.to_vec())
    }
}

/// Asserts that a pack index that fails to record cells never fails the publish: the base is
/// published, written and listed, and no cells are counted recorded. Catches: a start that
/// refuses to serve because the cells of its base could not be recorded.
#[tokio::test]
async fn an_index_that_fails_the_cells_never_fails_the_publish() {
    let dir = scratch("base-publish-cells-fail");
    let release = standard(b"kbf-task of failed cells");
    install(&dir, &release);
    let store = Arc::new(Bucket::default());
    let sidecars = Sidecars::new(Arc::new(NoCells), store.clone());

    let published = publish(
        &dir,
        COMMIT,
        PLATFORM,
        store.clone(),
        true,
        &retry(),
        Some(&sidecars),
    )
    .await
    .expect("published");

    assert_eq!(published.cells_recorded, 0);
    assert_eq!(published.written, release.objects.len() as u64);
    assert!(published.base.is_listed());
}
