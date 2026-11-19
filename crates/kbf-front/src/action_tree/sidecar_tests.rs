//! The L2 sidecar: dual emission keeps the tree id and reads no object, a list naming an object
//! with no cells is recorded unlisted, and the miss path and backfill read each object once.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use kbf_objstore::{BodyStream, ObjectKey, ObjectStoreError};
use kbf_pack_index::PackIndex;
use kbf_pack_index::cells::ObjectCells;
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_state::ManualClock;
use sha2::{Digest as _, Sha256};
use tierstore_list::TreeListL2;

use super::sidecar::{Backfilled, Emitted, READ_WINDOW, SidecarError, Sidecars, read_cells};
use super::tstree::{Child, Item, RANGE_MERGE_GAP, encode};
use crate::large_backfill::LargeStore;

/// An object store in memory: create-only, counting what it is asked.
#[derive(Default)]
struct Objects {
    held: Mutex<BTreeMap<String, Bytes>>,
    /// Every read: the key, the offset and the length.
    reads: Mutex<Vec<(String, u64, u64)>>,
    /// Every key a put was asked for.
    puts: Mutex<Vec<String>>,
    /// Every key a HEAD was asked for.
    heads: Mutex<Vec<String>>,
    /// Keys whose reads answer 404 though a HEAD finds them.
    vanishing: Mutex<Vec<String>>,
    /// Keys whose reads answer one byte short.
    short: Mutex<Vec<String>>,
    /// Puts fail with this.
    failing: Mutex<Option<ObjectStoreError>>,
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Objects {
    fn hold(&self, key: &str, bytes: Vec<u8>) {
        locked(&self.held).insert(key.to_owned(), Bytes::from(bytes));
    }

    fn get(&self, key: &str) -> Option<Bytes> {
        locked(&self.held).get(key).cloned()
    }

    /// The bytes read of `key`, by the reads asked.
    fn read_of(&self, key: &str) -> u64 {
        locked(&self.reads)
            .iter()
            .filter(|(read, _, _)| read == key)
            .map(|(_, _, len)| len)
            .sum()
    }
}

impl LargeStore for Objects {
    fn len<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> BoxFuture<'a, Result<Option<u64>, ObjectStoreError>> {
        locked(&self.heads).push(key.to_string());
        let len = self.get(key.as_str()).map(|bytes| bytes.len() as u64);
        Box::pin(std::future::ready(Ok(len)))
    }

    fn put<'a>(
        &'a self,
        key: &'a ObjectKey,
        len: u64,
        body: BodyStream,
    ) -> BoxFuture<'a, Result<(), ObjectStoreError>> {
        Box::pin(async move {
            locked(&self.puts).push(key.to_string());
            if let Some(error) = locked(&self.failing).clone() {
                return Err(error);
            }
            let pieces: Vec<Result<Bytes, ObjectStoreError>> = body.collect().await;
            let mut bytes = Vec::new();
            for piece in pieces {
                bytes.extend_from_slice(&piece?);
            }
            assert_eq!(bytes.len() as u64, len, "a put's body is its length");
            let mut held = locked(&self.held);
            if held.contains_key(key.as_str()) {
                return Err(ObjectStoreError::AlreadyExists(key.clone()));
            }
            held.insert(key.to_string(), Bytes::from(bytes));
            Ok(())
        })
    }

    fn read_range<'a>(
        &'a self,
        key: &'a ObjectKey,
        offset: u64,
        len: u64,
    ) -> BoxFuture<'a, Result<Bytes, ObjectStoreError>> {
        locked(&self.reads).push((key.to_string(), offset, len));
        let vanished = locked(&self.vanishing).iter().any(|gone| gone == key.as_str());
        let short = locked(&self.short).iter().any(|cut| cut == key.as_str()) as u64;
        let answer = match (vanished, self.get(key.as_str())) {
            (false, Some(bytes)) => {
                let (start, end) = (offset as usize, (offset + len - short) as usize);
                Ok(bytes.slice(start..end))
            }
            _ => Err(ObjectStoreError::NotFound(key.clone())),
        };
        Box::pin(std::future::ready(answer))
    }
}

/// A file of the test tree: its name, the object its bytes are in, the offset there and the size.
struct File {
    name: &'static str,
    key: &'static str,
    offset: u64,
    size: u64,
}

/// The version 2 list of a root holding `files`, as the writer encodes it.
fn list(files: &[File]) -> Vec<u8> {
    let children = |root: bool| -> Vec<Child<'_, bool>> {
        if !root {
            return Vec::new();
        }
        files
            .iter()
            .map(|file| Child {
                name: file.name,
                item: Item::File {
                    mode: 0o644,
                    size: file.size,
                    sha256: [7; 32],
                    source: Some((Cow::Borrowed(file.key), file.offset)),
                },
            })
            .collect()
    };
    encode(true, children, RANGE_MERGE_GAP, usize::MAX).expect("a list")
}

/// `size` bytes, each the low byte of its offset plus `seed`.
fn object(size: u64, seed: u8) -> Vec<u8> {
    (0..size).map(|at| (at as u8).wrapping_add(seed)).collect()
}

/// The cells of `bytes`, by an independent SHA-256.
fn cells_of(bytes: &[u8]) -> ObjectCells {
    ObjectCells {
        size: bytes.len() as u64,
        digests: bytes
            .chunks(1 << 20)
            .map(|cell| Sha256::digest(cell).into())
            .collect(),
    }
}

/// The tree id of `v2`.
fn tree_of(v2: &[u8]) -> [u8; 32] {
    Sha256::digest(v2).into()
}

/// The sidecar key of `v2`.
fn sidecar_of(v2: &[u8]) -> String {
    tierstore_list::sidecar_key(&tree_of(v2))
}

struct Harness {
    clock: Arc<ManualClock>,
    index: Arc<MemoryPackIndex>,
    objects: Arc<Objects>,
    sidecars: Sidecars,
}

fn harness() -> Harness {
    let clock = Arc::new(ManualClock::default());
    let index = Arc::new(MemoryPackIndex::new(clock.clone()));
    let objects = Arc::new(Objects::default());
    let sidecars = Sidecars::new(index.clone(), objects.clone());
    Harness {
        clock,
        index,
        objects,
        sidecars,
    }
}

/// Two packs and a whole object, the first pack named by two files.
fn three_objects(h: &Harness) -> (Vec<u8>, [Vec<u8>; 3]) {
    let a = object(3 * (1 << 20) + 5, 1);
    let b = object(1 << 20, 2);
    let c = object(17, 3);
    h.objects.hold("cas/f/w/b/1", a.clone());
    h.objects.hold("cas/f/w/b/2", b.clone());
    h.objects.hold("cas/l/cc", c.clone());
    let v2 = list(&[
        File {
            name: "a",
            key: "cas/f/w/b/1",
            offset: 0,
            size: 10,
        },
        File {
            name: "b",
            key: "cas/f/w/b/2",
            offset: 4,
            size: 100,
        },
        File {
            name: "c",
            key: "cas/l/cc",
            offset: 0,
            size: 17,
        },
        File {
            name: "d",
            key: "cas/f/w/b/1",
            offset: 1 << 20,
            size: 9,
        },
    ]);
    let id: String = tree_of(&v2).iter().map(|b| format!("{b:02x}")).collect();
    h.objects.hold(&format!("cas/l/{id}"), v2.clone());
    (v2, [a, b, c])
}

fn record_all(h: &Harness, objects: &[Vec<u8>; 3]) {
    for (key, bytes) in ["cas/f/w/b/1", "cas/f/w/b/2", "cas/l/cc"].iter().zip(objects) {
        assert!(h.index.record_cells(key, &cells_of(bytes)).expect("recorded"));
    }
}

#[tokio::test]
async fn dual_emission_writes_a_sidecar_whose_tree_id_is_the_lists_and_reads_no_object() {
    let h = harness();
    let (v2, objects) = three_objects(&h);
    record_all(&h, &objects);

    assert_eq!(h.sidecars.emit(&v2).await.expect("emitted"), Emitted::Written);

    let sidecar = h.objects.get(&sidecar_of(&v2)).expect("the sidecar is stored");
    let opened = TreeListL2::open_sidecar(Some(&sidecar[..]), &tree_of(&v2)).expect("it opens");
    assert_eq!(opened.projection_digest(), tree_of(&v2), "its projection is the list");
    assert!(locked(&h.objects.reads).is_empty(), "no object is read");
    let expected = tierstore_list::sidecar_from_v2(&v2, |key| {
        let bytes = match key {
            "cas/f/w/b/1" => &objects[0],
            "cas/f/w/b/2" => &objects[1],
            _ => &objects[2],
        };
        Some(tierstore_list::PackInfo {
            cells: tierstore_list::PackCells::of(bytes),
            upload_part_size: 0,
            shared: false,
        })
    })
    .expect("a sidecar");
    assert_eq!(&sidecar[..], &expected[..], "the cells are the objects' own");
}

#[tokio::test]
async fn a_list_naming_an_object_with_no_cells_gets_no_sidecar_and_is_recorded_unlisted() {
    let h = harness();
    let (v2, objects) = three_objects(&h);
    h.index
        .record_cells("cas/f/w/b/2", &cells_of(&objects[1]))
        .expect("recorded");

    let emitted = h.sidecars.emit(&v2).await.expect("emitted");

    assert_eq!(emitted, Emitted::Unlisted { missing: 2 });
    assert!(h.objects.get(&sidecar_of(&v2)).is_none(), "no sidecar");
    assert!(locked(&h.objects.reads).is_empty(), "no object is read");
    let rows = h.index.unlisted(Duration::from_secs(60), None, 10).expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tree, tree_of(&v2));
    let mut text = String::new();
    h.sidecars.counters().render(&mut text);
    assert!(text.contains("kbf_l2_sidecar_missing_total 1\n"), "{text}");
}

#[tokio::test]
async fn a_sidecar_already_written_is_kept_and_nothing_is_asked_of_the_index() {
    let h = harness();
    let (v2, _) = three_objects(&h);
    h.objects.hold(&sidecar_of(&v2), b"the first sidecar".to_vec());

    assert_eq!(h.sidecars.emit(&v2).await.expect("emitted"), Emitted::Written);

    assert_eq!(
        h.objects.get(&sidecar_of(&v2)).as_deref(),
        Some(&b"the first sidecar"[..])
    );
    assert!(locked(&h.objects.puts).is_empty(), "nothing is written");
    let rows = h.index.unlisted(Duration::from_secs(60), None, 10).expect("rows");
    assert!(rows.is_empty(), "an object with no cells was not even looked at");
}

#[tokio::test]
async fn backfill_reads_each_object_with_no_cells_once_and_writes_the_sidecar() {
    let h = harness();
    let (v2, objects) = three_objects(&h);
    h.index
        .record_cells("cas/f/w/b/2", &cells_of(&objects[1]))
        .expect("recorded");
    assert!(matches!(
        h.sidecars.emit(&v2).await,
        Ok(Emitted::Unlisted { .. })
    ));

    let done = h.sidecars.backfill(&v2).await.expect("backfilled");

    let (a, c) = (objects[0].len() as u64, objects[2].len() as u64);
    assert_eq!(
        done,
        Backfilled {
            objects_read: 2,
            bytes_read: a + c
        }
    );
    assert_eq!(h.objects.read_of("cas/f/w/b/1"), a, "the pack named twice is read once");
    assert_eq!(h.objects.read_of("cas/l/cc"), c);
    assert_eq!(h.objects.read_of("cas/f/w/b/2"), 0, "an object with cells is not read");
    let reads = locked(&h.objects.reads).clone();
    let of_a: Vec<(u64, u64)> = reads
        .iter()
        .filter(|(key, _, _)| key == "cas/f/w/b/1")
        .map(|(_, offset, len)| (*offset, *len))
        .collect();
    assert_eq!(of_a, vec![(0, a)], "one window: the pack is under one window");
    let cells = h
        .index
        .object_cells(&["cas/f/w/b/1".to_owned(), "cas/l/cc".to_owned()])
        .expect("cells");
    assert_eq!(cells, vec![Some(cells_of(&objects[0])), Some(cells_of(&objects[2]))]);
    let sidecar = h.objects.get(&sidecar_of(&v2)).expect("the sidecar is stored");
    TreeListL2::open_sidecar(Some(&sidecar[..]), &tree_of(&v2)).expect("it opens");
    let rows = h.index.unlisted(Duration::from_secs(60), None, 10).expect("rows");
    assert!(rows.is_empty(), "the tree is no longer unlisted");
}

#[tokio::test]
async fn a_second_list_over_the_same_pack_reads_nothing_more() {
    let h = harness();
    let (v2, _) = three_objects(&h);
    h.sidecars.backfill(&v2).await.expect("backfilled");
    let read = locked(&h.objects.reads).len();
    let other = list(&[File {
        name: "again",
        key: "cas/f/w/b/1",
        offset: 3,
        size: 4,
    }]);

    let done = h.sidecars.backfill(&other).await.expect("backfilled");

    assert_eq!(done, Backfilled::default());
    assert_eq!(locked(&h.objects.reads).len(), read, "no read of a pack with cells");
    assert!(h.objects.get(&sidecar_of(&other)).is_some());
}

#[tokio::test]
async fn an_object_the_store_does_not_have_is_lost_and_no_sidecar_is_written() {
    let h = harness();
    let (v2, _) = three_objects(&h);
    locked(&h.objects.held).remove("cas/l/cc");

    let error = h.sidecars.backfill(&v2).await.expect_err("lost");

    assert!(
        matches!(&error, SidecarError::Lost { key } if key == "cas/l/cc"),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "the store has no object cas/l/cc the tree list names"
    );
    assert!(h.objects.get(&sidecar_of(&v2)).is_none());
}

#[tokio::test]
async fn an_object_that_vanishes_between_its_head_and_its_read_is_lost() {
    let h = harness();
    let (v2, _) = three_objects(&h);
    locked(&h.objects.vanishing).push("cas/f/w/b/1".to_owned());

    let error = h.sidecars.backfill(&v2).await.expect_err("lost");

    assert!(matches!(&error, SidecarError::Lost { key } if key == "cas/f/w/b/1"));
    let mut text = String::new();
    h.sidecars.counters().render(&mut text);
    assert!(text.contains("kbf_l2_sidecars_total{result=\"failed\"} 1\n"), "{text}");
}

#[tokio::test]
async fn a_read_answered_short_is_a_store_failure_and_records_no_cells() {
    let h = harness();
    let (v2, _) = three_objects(&h);
    locked(&h.objects.short).push("cas/f/w/b/1".to_owned());

    let error = h.sidecars.backfill(&v2).await.expect_err("short");

    assert!(matches!(error, SidecarError::Store(ObjectStoreError::Body(_))), "{error:?}");
    let cells = h.index.object_cells(&["cas/f/w/b/1".to_owned()]).expect("cells");
    assert_eq!(cells, vec![None]);
}

#[tokio::test]
async fn bytes_that_are_not_a_tree_list_are_refused_by_name() {
    let h = harness();

    let error = h.sidecars.emit(b"not a list").await.expect_err("refused");

    assert!(matches!(error, SidecarError::NotAList(_)), "{error:?}");
    let error = h.sidecars.backfill(b"not a list").await.expect_err("refused");
    assert!(matches!(error, SidecarError::NotAList(_)), "{error:?}");
}

#[tokio::test]
async fn a_failed_write_of_the_sidecar_is_the_stores_error_and_is_counted() {
    let h = harness();
    let (v2, objects) = three_objects(&h);
    record_all(&h, &objects);
    *locked(&h.objects.failing) = Some(ObjectStoreError::Protocol("refused".to_owned()));

    let error = h.sidecars.emit(&v2).await.expect_err("failed");

    assert!(matches!(error, SidecarError::Store(ObjectStoreError::Protocol(_))), "{error:?}");
    let mut text = String::new();
    h.sidecars.counters().render(&mut text);
    assert!(text.contains("kbf_l2_sidecars_total{result=\"failed\"} 1\n"), "{text}");
}

#[tokio::test]
async fn an_index_that_keeps_no_cells_fails_the_sidecar_and_never_the_caller() {
    struct NoCells;
    impl PackIndex for NoCells {
        fn record_pack(
            &self,
            _: &kbf_pack_index::NewPack,
        ) -> Result<kbf_pack_index::Recorded, kbf_pack_index::PackIndexError> {
            Err(kbf_pack_index::PackIndexError::Unsupported)
        }
        fn pack(
            &self,
            _: &str,
        ) -> Result<Option<kbf_pack_index::PackInfo>, kbf_pack_index::PackIndexError> {
            Ok(None)
        }
        fn locate(
            &self,
            digests: &[kbf_pack_index::BlobDigest],
        ) -> Result<Vec<Option<kbf_pack_index::Location>>, kbf_pack_index::PackIndexError>
        {
            Ok(vec![None; digests.len()])
        }
        fn find_missing(
            &self,
            digests: &[kbf_pack_index::BlobDigest],
        ) -> Result<Vec<kbf_pack_index::BlobDigest>, kbf_pack_index::PackIndexError> {
            Ok(digests.to_vec())
        }
        fn touch(
            &self,
            _: &[kbf_pack_index::BlobDigest],
        ) -> Result<usize, kbf_pack_index::PackIndexError> {
            Ok(0)
        }
        fn dead_packs(
            &self,
            _: Duration,
            _: usize,
        ) -> Result<Vec<kbf_pack_index::PackInfo>, kbf_pack_index::PackIndexError> {
            Ok(Vec::new())
        }
        fn unreferenced_keys(
            &self,
            keys: &[String],
        ) -> Result<Vec<String>, kbf_pack_index::PackIndexError> {
            Ok(keys.to_vec())
        }
    }
    let objects = Arc::new(Objects::default());
    let sidecars = Sidecars::new(Arc::new(NoCells), objects.clone());
    let v2 = list(&[File {
        name: "a",
        key: "cas/l/aa",
        offset: 0,
        size: 3,
    }]);

    let error = sidecars.emit(&v2).await.expect_err("no cells");
    assert!(matches!(error, SidecarError::Index(_)), "{error:?}");
    let key = ObjectKey::new("cas/l/aa".to_owned()).expect("a key");
    // Logged and dropped: nothing to see but that it returns.
    sidecars.record(&key, cells_of(b"abc")).await;
}

#[tokio::test]
async fn record_keeps_the_cells_of_an_object_just_written() {
    let h = harness();
    let key = ObjectKey::new("cas/l/dd".to_owned()).expect("a key");

    h.sidecars.record(&key, cells_of(b"four")).await;

    let cells = h.index.object_cells(&["cas/l/dd".to_owned()]).expect("cells");
    assert_eq!(cells, vec![Some(cells_of(b"four"))]);
}

#[tokio::test]
async fn read_cells_reads_in_windows_back_to_back_and_hashes_every_cell() {
    let objects = Objects::default();
    for (key, size) in [
        ("cas/l/one", 1),
        ("cas/l/window", READ_WINDOW),
        ("cas/l/over", READ_WINDOW + 1),
        ("cas/l/cell", (1 << 20) + 1),
    ] {
        objects.hold(key, object(size, 9));
        let object_key = ObjectKey::new(key.to_owned()).expect("a key");

        let cells = read_cells(&objects, &object_key).await.expect("cells");

        assert_eq!(cells, cells_of(&object(size, 9)), "{key}");
        let windows: Vec<(u64, u64)> = locked(&objects.reads)
            .iter()
            .filter(|(read, _, _)| read == key)
            .map(|(_, offset, len)| (*offset, *len))
            .collect();
        let mut expected = Vec::new();
        let mut at = 0;
        while at < size {
            let len = READ_WINDOW.min(size - at);
            expected.push((at, len));
            at += len;
        }
        assert_eq!(windows, expected, "{key}");
    }
}

#[tokio::test]
async fn the_counters_render_every_family() {
    let h = harness();
    let (v2, objects) = three_objects(&h);
    record_all(&h, &objects);
    h.sidecars.emit(&v2).await.expect("written");
    h.sidecars.emit(&v2).await.expect("present");
    h.clock.advance(Duration::from_secs(1));

    let mut text = "# before\n".to_owned();
    h.sidecars.counters().render(&mut text);

    assert_eq!(
        text,
        "# before\n\
         # HELP kbf_l2_sidecars_total L2 sidecars of tree lists, by how their write ended: \
         written, already present, failed.\n\
         # TYPE kbf_l2_sidecars_total counter\n\
         kbf_l2_sidecars_total{result=\"written\"} 1\n\
         kbf_l2_sidecars_total{result=\"present\"} 1\n\
         kbf_l2_sidecars_total{result=\"failed\"} 0\n\
         # HELP kbf_l2_sidecar_missing_total Tree lists written with no L2 sidecar, because an \
         object they name has no recorded cells: each is left to the backfill.\n\
         # TYPE kbf_l2_sidecar_missing_total counter\n\
         kbf_l2_sidecar_missing_total 0\n\
         # HELP kbf_l2_cells_read_objects_total Objects read once for their cell digests, by \
         the miss path or the backfill.\n\
         # TYPE kbf_l2_cells_read_objects_total counter\n\
         kbf_l2_cells_read_objects_total 0\n\
         # HELP kbf_l2_cells_read_bytes_total Bytes read for those cell digests.\n\
         # TYPE kbf_l2_cells_read_bytes_total counter\n\
         kbf_l2_cells_read_bytes_total 0\n"
    );
}
