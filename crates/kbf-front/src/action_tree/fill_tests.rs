//! The fill: a list dual emission leaves unlisted gets its sidecar from one task after its
//! write, each tree once and within the queue's limit, a fill that fails keeps the tree's row,
//! and the cells it records give the next list over the same objects its sidecar at its write.
//! And the counts: every list once in `kbf_l2_tree_lists_total`, and the cells recorded from
//! bytes in hand.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use kbf_objstore::ObjectStoreError;
use kbf_pack_index::PackIndex;
use tierstore_list::TreeListL2;

use super::sidecar::{Emitted, Sidecars};
use super::sidecar_tests::{
    File, Harness, cells_of, harness, list, locked, object, record_all, sidecar_of, three_objects,
    tree_of,
};

/// The harness, its sidecars filling with a queue of `limit` bytes.
fn filling(limit: u64) -> Harness {
    let mut h = harness();
    h.sidecars = h.sidecars.clone().with_fills(limit);
    h
}

/// The counters' text.
fn rendered(sidecars: &Sidecars) -> String {
    let mut text = String::new();
    sidecars.counters().render(&mut text);
    text
}

/// The unlisted rows of the index.
fn unlisted(h: &Harness) -> usize {
    h.index
        .unlisted(Duration::from_secs(60), None, 10)
        .expect("rows")
        .len()
}

/// A list over the objects of [`three_objects`] under other names: another tree.
fn another_list() -> Vec<u8> {
    list(&[
        File {
            name: "x",
            key: "cas/f/w/b/1",
            offset: 0,
            size: 10,
        },
        File {
            name: "y",
            key: "cas/f/w/b/2",
            offset: 4,
            size: 100,
        },
        File {
            name: "z",
            key: "cas/l/cc",
            offset: 0,
            size: 17,
        },
    ])
}

/// Catches: a list left unlisted that never gets its sidecar until an operator runs the
/// backfill, and cells read for one list and not kept for the next. The list is unlisted at its
/// write; once the fill settles its sidecar is the one the cells make, its row is dropped and
/// each object was read once; a second list over the same objects gets its sidecar at its write
/// and reads nothing.
#[tokio::test]
async fn a_list_left_unlisted_is_filled_and_the_next_list_over_its_objects_has_its_sidecar_at_its_write()
 {
    let h = filling(1 << 20);
    let (v2, objects) = three_objects(&h);

    let emitted = h.sidecars.emit(&v2).await.expect("emitted");
    assert_eq!(emitted, Emitted::Unlisted { missing: 3 });
    h.sidecars.settled().await;

    let sidecar = h
        .objects
        .get(&sidecar_of(&v2))
        .expect("the sidecar is filled");
    let opened = TreeListL2::open_sidecar(Some(&sidecar[..]), &tree_of(&v2)).expect("it opens");
    assert_eq!(opened.projection_digest(), tree_of(&v2));
    assert_eq!(unlisted(&h), 0, "the row is dropped");
    for (key, bytes) in ["cas/f/w/b/1", "cas/f/w/b/2", "cas/l/cc"]
        .iter()
        .zip(&objects)
    {
        assert_eq!(
            h.objects.read_of(key),
            bytes.len() as u64,
            "{key} read once"
        );
    }
    let reads = locked(&h.objects.reads).len();

    let next = another_list();
    assert_eq!(
        h.sidecars.emit(&next).await.expect("emitted"),
        Emitted::Written
    );
    h.sidecars.settled().await;

    assert!(h.objects.get(&sidecar_of(&next)).is_some());
    assert_eq!(
        locked(&h.objects.reads).len(),
        reads,
        "nothing more is read"
    );
    let text = rendered(&h.sidecars);
    assert!(
        text.contains("kbf_l2_sidecar_fills_total{result=\"written\"} 1\n"),
        "{text}"
    );
    assert!(
        text.contains("kbf_l2_tree_lists_total{sidecar=\"missing\"} 1\n"),
        "{text}"
    );
    assert!(
        text.contains("kbf_l2_tree_lists_total{sidecar=\"written\"} 1\n"),
        "{text}"
    );
}

/// Catches: a tree filled once for each time it was queued, and bytes of a filled list still
/// held against the limit. The queue's limit is the bytes of two lists: the first queued twice
/// and the second once are each filled once; once that settled, both are queued again and fit,
/// so nothing is dropped.
#[tokio::test]
async fn each_tree_is_queued_once_and_the_bytes_of_a_filled_list_are_given_back() {
    let probe = harness();
    let (first, _) = three_objects(&probe);
    let second = another_list();
    let h = filling((first.len() + second.len()) as u64);
    let (first, _) = three_objects(&h);

    h.sidecars.queue_fill(tree_of(&first), &first);
    h.sidecars.queue_fill(tree_of(&first), &first);
    h.sidecars.queue_fill(tree_of(&second), &second);
    h.sidecars.settled().await;

    assert!(h.objects.get(&sidecar_of(&first)).is_some());
    assert!(h.objects.get(&sidecar_of(&second)).is_some());
    let sidecar_puts = || {
        locked(&h.objects.puts)
            .iter()
            .filter(|key| key.starts_with("cas/l2/"))
            .count()
    };
    assert_eq!(sidecar_puts(), 2, "each sidecar is written once");

    h.sidecars.queue_fill(tree_of(&first), &first);
    h.sidecars.queue_fill(tree_of(&second), &second);
    h.sidecars.settled().await;

    assert_eq!(sidecar_puts(), 4, "both are filled again");
    let text = rendered(&h.sidecars);
    assert!(
        text.contains("kbf_l2_sidecar_fills_total{result=\"written\"} 4\n"),
        "{text}"
    );
    assert!(
        text.contains("kbf_l2_sidecar_fills_total{result=\"dropped\"} 0\n"),
        "{text}"
    );
}

/// Catches: a queue that grows past its limit, or a dropped list that loses its row. A list
/// longer than the limit is dropped and counted, nothing is read, and its row stays for the
/// backfill.
#[tokio::test]
async fn a_list_over_the_limit_is_dropped_and_keeps_its_row() {
    let probe = harness();
    let (v2, _) = three_objects(&probe);
    let h = filling(v2.len() as u64 - 1);
    let (v2, _) = three_objects(&h);

    let emitted = h.sidecars.emit(&v2).await.expect("emitted");
    h.sidecars.settled().await;

    assert_eq!(emitted, Emitted::Unlisted { missing: 3 });
    assert!(h.objects.get(&sidecar_of(&v2)).is_none(), "no sidecar");
    assert!(locked(&h.objects.reads).is_empty(), "nothing is read");
    assert_eq!(unlisted(&h), 1, "the row stays");
    let text = rendered(&h.sidecars);
    assert!(
        text.contains("kbf_l2_sidecar_fills_total{result=\"dropped\"} 1\n"),
        "{text}"
    );
}

/// Catches: a fill that writes a sidecar naming an object the store lost, or drops the tree's
/// row. The fill ends lost, counted, and the row stays.
#[tokio::test]
async fn a_fill_over_an_object_the_store_lost_is_counted_lost_and_keeps_its_row() {
    let h = filling(1 << 20);
    let v2 = list(&[File {
        name: "gone",
        key: "cas/l/gone",
        offset: 0,
        size: 3,
    }]);

    h.sidecars.emit(&v2).await.expect("emitted");
    h.sidecars.settled().await;

    assert!(h.objects.get(&sidecar_of(&v2)).is_none(), "no sidecar");
    assert_eq!(unlisted(&h), 1, "the row stays");
    let text = rendered(&h.sidecars);
    assert!(
        text.contains("kbf_l2_sidecar_fills_total{result=\"lost\"} 1\n"),
        "{text}"
    );
    assert!(
        text.contains("kbf_l2_sidecars_total{result=\"failed\"} 0\n"),
        "{text}"
    );
}

/// Catches: a store failure that fails the fill silently or drops the row. The fill ends
/// failed, counted in the fills and not as a list's failure, and the row stays.
#[tokio::test]
async fn a_fill_the_store_fails_is_counted_failed_and_keeps_its_row() {
    let h = filling(1 << 20);
    let (v2, objects) = three_objects(&h);
    record_all(&h, &objects);
    h.index.record_unlisted(&tree_of(&v2)).expect("unlisted");
    *locked(&h.objects.failing) = Some(ObjectStoreError::Protocol("refused".to_owned()));

    h.sidecars.queue_fill(tree_of(&v2), &v2);
    h.sidecars.settled().await;

    assert_eq!(unlisted(&h), 1, "the row stays");
    let text = rendered(&h.sidecars);
    assert!(
        text.contains("kbf_l2_sidecar_fills_total{result=\"failed\"} 1\n"),
        "{text}"
    );
    assert!(
        text.contains("kbf_l2_tree_lists_total{sidecar=\"failed\"} 0\n"),
        "{text}"
    );
}

/// Catches: sidecars that fill when they were not asked to. Without fills a list left unlisted
/// is not read, and `settled` returns at once.
#[tokio::test]
async fn without_fills_a_list_left_unlisted_is_not_read() {
    let h = harness();
    let (v2, _) = three_objects(&h);

    h.sidecars.emit(&v2).await.expect("emitted");
    h.sidecars.settled().await;

    assert!(h.objects.get(&sidecar_of(&v2)).is_none());
    assert!(locked(&h.objects.reads).is_empty());
    assert_eq!(unlisted(&h), 1);
}

/// Catches: a share that counts a list twice or not at all. Each way a list's write ends is
/// counted once, by whether it had its sidecar: written, present, missing, failed (the store
/// refused the sidecar), and the miss path's written and failed.
#[tokio::test]
async fn every_list_is_counted_once_by_whether_it_had_its_sidecar() {
    let h = harness();
    let (v2, objects) = three_objects(&h);
    let next = another_list();
    h.sidecars.emit(&v2).await.expect("missing");
    record_all(&h, &objects);
    h.sidecars.emit(&v2).await.expect("written");
    h.sidecars.emit(&v2).await.expect("present");
    h.sidecars.republish(&next).await.expect("republished");
    *locked(&h.objects.failing) = Some(ObjectStoreError::Protocol("refused".to_owned()));
    let third = list(&[File {
        name: "w",
        key: "cas/l/cc",
        offset: 0,
        size: 17,
    }]);
    h.sidecars.emit(&third).await.expect_err("failed");
    h.sidecars.republish(&[0; 3]).await.expect_err("not a list");

    let text = rendered(&h.sidecars);
    for (sidecar, count) in [
        ("written", 2),
        ("present", 1),
        ("missing", 1),
        ("failed", 2),
    ] {
        let line = format!("kbf_l2_tree_lists_total{{sidecar=\"{sidecar}\"}} {count}\n");
        assert!(text.contains(&line), "{line}{text}");
    }
}

/// Catches: cells recorded over those an index holds, or none recorded. `record_missing`
/// records the cells of the objects the index has none of, keeps the others, and counts them.
#[tokio::test]
async fn record_missing_records_only_the_objects_with_no_cells() {
    let h = harness();
    let (held, new) = (object(5, 1), object((1 << 20) + 1, 2));
    h.index
        .record_cells("cas/l/held", &cells_of(&held))
        .expect("recorded");
    let given = BTreeMap::from([
        ("cas/l/held".to_owned(), cells_of(&new)),
        ("cas/l/new".to_owned(), cells_of(&new)),
    ]);

    let recorded = h.sidecars.record_missing(given).await.expect("recorded");

    assert_eq!(recorded, 1);
    let keys = ["cas/l/held".to_owned(), "cas/l/new".to_owned()];
    assert_eq!(
        h.index.object_cells(&keys).expect("cells"),
        vec![Some(cells_of(&held)), Some(cells_of(&new))]
    );
    let text = rendered(&h.sidecars);
    assert!(text.contains("kbf_l2_cells_recorded_total 1\n"), "{text}");
}

/// A pack index that finds no cells and refuses to record any.
struct RefusesCells(kbf_pack_index::memory::MemoryPackIndex);

impl PackIndex for RefusesCells {
    fn record_pack(
        &self,
        pack: &kbf_pack_index::NewPack,
    ) -> Result<kbf_pack_index::Recorded, kbf_pack_index::PackIndexError> {
        self.0.record_pack(pack)
    }
    fn pack(
        &self,
        key: &str,
    ) -> Result<Option<kbf_pack_index::PackInfo>, kbf_pack_index::PackIndexError> {
        self.0.pack(key)
    }
    fn locate(
        &self,
        digests: &[kbf_pack_index::BlobDigest],
    ) -> Result<Vec<Option<kbf_pack_index::Location>>, kbf_pack_index::PackIndexError> {
        self.0.locate(digests)
    }
    fn find_missing(
        &self,
        digests: &[kbf_pack_index::BlobDigest],
    ) -> Result<Vec<kbf_pack_index::BlobDigest>, kbf_pack_index::PackIndexError> {
        self.0.find_missing(digests)
    }
    fn touch(
        &self,
        digests: &[kbf_pack_index::BlobDigest],
    ) -> Result<usize, kbf_pack_index::PackIndexError> {
        self.0.touch(digests)
    }
    fn dead_packs(
        &self,
        grace: Duration,
        limit: usize,
    ) -> Result<Vec<kbf_pack_index::PackInfo>, kbf_pack_index::PackIndexError> {
        self.0.dead_packs(grace, limit)
    }
    fn unreferenced_keys(
        &self,
        keys: &[String],
    ) -> Result<Vec<String>, kbf_pack_index::PackIndexError> {
        self.0.unreferenced_keys(keys)
    }
    fn object_cells(
        &self,
        keys: &[String],
    ) -> Result<Vec<Option<kbf_pack_index::cells::ObjectCells>>, kbf_pack_index::PackIndexError>
    {
        Ok(vec![None; keys.len()])
    }
    fn record_cells(
        &self,
        _: &str,
        _: &kbf_pack_index::cells::ObjectCells,
    ) -> Result<bool, kbf_pack_index::PackIndexError> {
        Err(kbf_pack_index::PackIndexError::Unsupported)
    }
}

/// Catches: a record that fails and is reported done. When the index refuses the record after
/// it answered that the object has no cells, `record_missing` is the index's error and nothing
/// is counted recorded.
#[tokio::test]
async fn record_missing_is_the_indexs_error_when_it_refuses_the_record() {
    let h = harness();
    let index = RefusesCells(kbf_pack_index::memory::MemoryPackIndex::new(
        h.clock.clone(),
    ));
    let sidecars = Sidecars::new(Arc::new(index), h.objects.clone());
    let given = BTreeMap::from([("cas/l/new".to_owned(), cells_of(&object(5, 1)))]);

    let error = sidecars.record_missing(given).await.expect_err("refused");

    assert!(
        matches!(error, super::sidecar::SidecarError::Index(_)),
        "{error:?}"
    );
    let text = rendered(&sidecars);
    assert!(text.contains("kbf_l2_cells_recorded_total 0\n"), "{text}");
}
