//! When a tree list's L2 sidecar is read and written beside the list's own write
//! (`TreeWriter::write_listing_with`): the sidecar's HEAD is asked while the list's write is held,
//! the sidecar and the unlisted row are written only once the list is stored, a list whose write
//! fails fails the tree with no sidecar and no row, a sidecar that fails never fails the list, and
//! the list, its sidecar and its blobs are timed as stages of their own.
//!
//! Writes are held by the fake store's gate and the write is polled by hand, so which writes are
//! open at each step is fixed, not raced. The times are read on tokio's paused clock, which moves
//! only by the fake store's fixed delay of each write.

use std::pin::pin;

use futures::poll;
use kbf_front::action_tree::{
    Listing, ListingWrite, SidecarCounters, Sidecars, WriteStage, read_cells,
};
use kbf_pack_index::PackIndex;
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_state::ManualClock;

use super::listing::listed_base;
use super::sidecars::{hold_named_objects, id_of, one_file_root};
use super::*;

/// The time each write of the store takes when the test sets [`Faults::delay`].
const WRITE: Duration = Duration::from_millis(10);

/// The objects every list of the rig names: the pack of the Action and Command, the base's pack
/// and the base's whole file.
fn named_objects() -> [String; 3] {
    [
        "cas/f/w/p9".to_owned(),
        "cas/f/base/p0".to_owned(),
        whole_key("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"),
    ]
}

/// A rig whose writer writes sidecars from a pack index of its own, counting into counters of
/// its own; the index and the counters.
fn counted_rig(rig: Rig) -> (Rig, Arc<MemoryPackIndex>, Arc<SidecarCounters>) {
    let mut rig = rig;
    let index = Arc::new(MemoryPackIndex::new(Arc::new(ManualClock::default())));
    let counters = Arc::new(SidecarCounters::default());
    let store = Arc::clone(&rig.farm) as Arc<dyn LargeStore>;
    let sidecars = Sidecars::new(Arc::clone(&index) as Arc<dyn PackIndex>, store)
        .with_counters(Arc::clone(&counters));
    let source = Arc::clone(&rig.farm) as Arc<dyn BlobSource>;
    let store = Arc::clone(&rig.farm) as Arc<dyn LargeStore>;
    let clock: kbf_front::Clock = Arc::new(|| UNIX_EPOCH + Duration::from_secs(START_SECS));
    rig.writer = TreeWriter::new(source, store, clock).with_sidecars(Arc::new(sidecars));
    (rig, index, counters)
}

/// Records in `index` the cells of every object the rig's lists name, read from its store.
async fn record_named_cells(rig: &Rig, index: &MemoryPackIndex) {
    for key in named_objects() {
        let object = ObjectKey::new(key.clone()).expect("a key");
        let cells = read_cells(&*rig.farm, &object).await.expect("cells");
        index.record_cells(&key, &cells).expect("recorded");
    }
}

/// The key of the one-file list the rig writes: the list a writer with no sidecars stores for
/// the same input root, which the sidecar does not change.
async fn list_key() -> String {
    let plain = Rig::new().await;
    let root = one_file_root(&plain).await;
    let listing = super::listing::list(&plain, &root)
        .await
        .expect("a listing");
    listing.key().to_owned()
}

/// What a counter of `counters` says, by its line without the value.
fn counted(counters: &SidecarCounters, line: &str) -> String {
    let mut page = String::new();
    counters.render(&mut page);
    page.lines()
        .find_map(|l| l.strip_prefix(line))
        .unwrap_or_else(|| panic!("no {line:?} in {page}"))
        .trim()
        .to_owned()
}

/// Writes the rig's list of `root`'s tree with dual emission, its stages timed into `timed`.
async fn write(
    rig: &Rig,
    root: &Digest,
    timed: &Arc<Mutex<Vec<(WriteStage, Duration)>>>,
) -> Result<Listing, NoTree> {
    let kept = rig.kept(root).await;
    let timed = Arc::clone(timed);
    let timer = move |stage, took| {
        let mut timed = timed.lock().unwrap_or_else(PoisonError::into_inner);
        timed.push((stage, took));
    };
    let base = listed_base(rig);
    let how = ListingWrite::default();
    rig.writer
        .write_listing_with(&kept, &base, how, |_| (), timer)
        .await
}

/// Catches a sidecar whose reads wait for the list's write, one store round trip more before
/// the tree is stored: with every write held, the list's write is the only one asked for and the
/// sidecar's HEAD has already been asked; once the list is let through, its sidecar follows it.
#[tokio::test]
async fn the_sidecar_head_is_asked_while_the_list_write_is_held() {
    let (rig, index, _) = counted_rig(Rig::new().await);
    hold_named_objects(&rig);
    record_named_cells(&rig, &index).await;
    let root = one_file_root(&rig).await;
    let kept = rig.kept(&root).await;
    let base = listed_base(&rig);
    let key = list_key().await;
    let sidecar = tierstore_list::sidecar_key(&id_of(&key));
    rig.farm.state().faults.hold_all = true;
    let how = ListingWrite::default();
    let mut writing = pin!(
        rig.writer
            .write_listing_with(&kept, &base, how, |_| (), |_, _| ())
    );

    assert!(poll!(writing.as_mut()).is_pending());

    assert_eq!(
        rig.farm.puts(),
        std::slice::from_ref(&key),
        "the list's write alone"
    );
    assert!(
        rig.farm.state().heads_of.contains(&sidecar),
        "the sidecar's HEAD is asked while the list's write is held"
    );
    rig.farm.gate.add_permits(2);
    let listing = writing.await.expect("a listing");
    assert_eq!(listing.key(), key);
    assert_eq!(rig.farm.puts(), [key, sidecar.clone()], "the sidecar after");
    assert!(rig.farm.object(&sidecar).is_some(), "the sidecar is stored");
}

/// Catches a sidecar or an unlisted row written for a list the store does not have, and a list
/// whose failed write is answered as stored: with the list's write refused, the tree fails as
/// the store's, and neither the sidecar (its cells recorded) nor the unlisted row (its cells not)
/// is written, and nothing is counted as a sidecar written or failed.
#[tokio::test]
async fn a_failed_list_write_fails_the_tree_and_writes_no_sidecar_and_no_row() {
    let key = list_key().await;
    let sidecar = tierstore_list::sidecar_key(&id_of(&key));
    for cells in [true, false] {
        let (rig, index, counters) = counted_rig(Rig::new().await);
        hold_named_objects(&rig);
        if cells {
            record_named_cells(&rig, &index).await;
        }
        let root = one_file_root(&rig).await;
        rig.farm.state().faults.put = Some(key.clone());
        let timed = Arc::default();

        let failed = write(&rig, &root, &timed).await;

        let Err(NoTree::Put { key: refused, .. }) = &failed else {
            panic!("the list's write failed the tree: {failed:?}");
        };
        assert_eq!(refused, &key, "cells {cells}");
        assert_eq!(failed.as_ref().map(drop).map_err(NoTree::why), Err("store"));
        assert!(
            rig.farm.object(&sidecar).is_none(),
            "no sidecar, cells {cells}"
        );
        let rows = index
            .unlisted(Duration::from_secs(60), None, 10)
            .expect("rows");
        assert!(rows.is_empty(), "no unlisted row, cells {cells}");
        for result in ["written", "present", "failed"] {
            let line = format!("kbf_l2_sidecars_total{{result=\"{result}\"}}");
            assert_eq!(counted(&counters, &line), "0", "{result}, cells {cells}");
        }
        let stages: Vec<WriteStage> = timed.lock().expect("timed").iter().map(|t| t.0).collect();
        assert_eq!(stages, [WriteStage::List], "cells {cells}");
    }
}

/// Catches a sidecar failure that fails the list: with the sidecar's write refused, the list is
/// stored and returned, the sidecar is not, and the failure is counted.
#[tokio::test]
async fn a_sidecar_that_fails_still_returns_the_stored_list() {
    let key = list_key().await;
    let sidecar = tierstore_list::sidecar_key(&id_of(&key));
    let (rig, index, counters) = counted_rig(Rig::new().await);
    hold_named_objects(&rig);
    record_named_cells(&rig, &index).await;
    let root = one_file_root(&rig).await;
    rig.farm.state().faults.put = Some(sidecar.clone());
    let timed = Arc::default();

    let listing = write(&rig, &root, &timed).await.expect("a listing");

    assert_eq!(listing.key(), key);
    assert!(rig.farm.object(&key).is_some(), "the list is stored");
    assert!(rig.farm.object(&sidecar).is_none(), "no sidecar");
    let failed = "kbf_l2_sidecars_total{result=\"failed\"}";
    assert_eq!(counted(&counters, failed), "1");
}

/// Catches a tree list's stages not timed, timed twice, or its sidecar timed as if it began after
/// the list's write: a list over a blob that was nowhere times its blobs, its list and its
/// sidecar once each, in that order; the list's write takes one write's time, and the sidecar,
/// begun with the list's write and written after it, two.
#[tokio::test(start_paused = true)]
async fn a_tree_list_times_its_blobs_its_list_and_its_sidecar() {
    let (rig, index, counters) = counted_rig(Rig::new().await);
    hold_named_objects(&rig);
    record_named_cells(&rig, &index).await;
    let blob = rig.farm.cas("only in a storage node's write tier").await;
    let root = rig
        .store(&directory(vec![file("f", &blob, false)], vec![], vec![]))
        .await;
    rig.farm.state().faults.delay = Some(WRITE);
    let timed = Arc::default();

    write(&rig, &root, &timed).await.expect("a listing");

    let timed = timed.lock().expect("timed").clone();
    assert_eq!(
        timed,
        [
            (WriteStage::Blobs, WRITE),
            (WriteStage::List, WRITE),
            (WriteStage::Sidecar, 2 * WRITE),
        ]
    );
    let written = "kbf_l2_sidecars_total{result=\"written\"}";
    assert_eq!(counted(&counters, written), "1", "the sidecar is written");
}
