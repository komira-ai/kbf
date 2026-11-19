//! The L2 sidecar of each tree list the writer stores (`TreeWriter::with_sidecars`): dual
//! emission keeps the tree id, a list naming objects with no cells is left unlisted, the miss
//! path (`ListingWrite::republish`) reads each such object once and writes the sidecar, the
//! cells of a whole object the writer writes are recorded as it streams, and nothing the sidecar
//! does fails the list.

use kbf_front::action_tree::{ListingWrite, Sidecars};
use kbf_pack_index::PackIndex;
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_state::ManualClock;
use sha2::{Digest as _, Sha256};
use tierstore_list::TreeListL2;

use super::listing::listed_base;
use super::*;

/// A rig whose writer writes sidecars from a pack index of its own, and the index.
async fn sidecar_rig() -> (Rig, Arc<MemoryPackIndex>) {
    let mut rig = Rig::new().await;
    let index = Arc::new(MemoryPackIndex::new(Arc::new(ManualClock::default())));
    let store = Arc::clone(&rig.farm) as Arc<dyn LargeStore>;
    let sidecars = Sidecars::new(Arc::clone(&index) as Arc<dyn PackIndex>, store);
    let source = Arc::clone(&rig.farm) as Arc<dyn BlobSource>;
    let store = Arc::clone(&rig.farm) as Arc<dyn LargeStore>;
    let clock: kbf_front::Clock = Arc::new(|| UNIX_EPOCH + Duration::from_secs(START_SECS));
    rig.writer = TreeWriter::new(source, store, clock).with_sidecars(Arc::new(sidecars));
    (rig, index)
}

/// The packs and whole objects every list of the rig names, held with bytes so they can be read:
/// the pack of the Action and Command, the base's pack and the base's whole file.
fn hold_named_objects(rig: &Rig) {
    let mut state = rig.farm.state();
    state
        .objects
        .insert("cas/f/w/p9".to_owned(), Bytes::from(vec![9u8; 4096]));
    state
        .objects
        .insert("cas/f/base/p0".to_owned(), Bytes::from(vec![1u8; 64]));
    state.objects.insert(
        whole_key("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"),
        Bytes::from_static(b"hello"),
    );
}

/// The id a list key `cas/l/<hex>` names.
fn id_of(key: &str) -> [u8; 32] {
    let hex = key.strip_prefix("cas/l/").expect("a list key");
    let mut id = [0u8; 32];
    for (at, byte) in id.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * at..2 * at + 2], 16).expect("hex");
    }
    id
}

/// An input root of one packed file.
async fn one_file_root(rig: &Rig) -> Digest {
    let blob = rig.farm.packed("int main;", "cas/f/w/p9", 3000).await;
    rig.store(&directory(vec![file("a.c", &blob, false)], vec![], vec![]))
        .await
}

/// Catches: a writer whose dual emission changes the list (and so the tree id) or names the
/// sidecar as the tree: the key and bytes of the version 2 list are those a writer with no
/// sidecars stores, and the sidecar is at `cas/l2/<the same id>`.
#[tokio::test]
async fn dual_emission_keeps_the_tree_id() {
    let plain = Rig::new().await;
    let root = one_file_root(&plain).await;
    let kept = plain.kept(&root).await;
    let without = plain
        .writer
        .write_listing(&kept, &listed_base(&plain))
        .await
        .expect("a listing");
    let (rig, _) = sidecar_rig().await;
    hold_named_objects(&rig);
    let root = one_file_root(&rig).await;
    let kept = rig.kept(&root).await;
    let how = ListingWrite {
        checked: false,
        republish: true,
    };

    let with = rig
        .writer
        .write_listing_with(&kept, &listed_base(&rig), how, |_| ())
        .await
        .expect("a listing");

    assert_eq!(with.key(), without.key(), "the tree id does not change");
    assert_eq!(
        rig.farm.object(with.key()),
        plain.farm.object(without.key()),
        "the version 2 list is the same bytes"
    );
    let sidecar = rig
        .farm
        .object(&tierstore_list::sidecar_key(&id_of(with.key())))
        .expect("the sidecar is stored");
    let opened =
        TreeListL2::open_sidecar(Some(&sidecar[..]), &id_of(with.key())).expect("it opens");
    assert_eq!(opened.projection_digest(), id_of(with.key()));
}

/// Catches: a writer that reads objects to emit a sidecar, or fails the list when it cannot: a
/// list naming objects with no recorded cells is stored, gets no sidecar, reads nothing, and its
/// tree is recorded unlisted.
#[tokio::test]
async fn a_list_over_objects_with_no_cells_is_stored_and_left_unlisted() {
    let (rig, index) = sidecar_rig().await;
    hold_named_objects(&rig);
    let root = one_file_root(&rig).await;

    let listing = super::listing::list(&rig, &root).await.expect("a listing");

    assert!(rig.farm.object(listing.key()).is_some(), "the list is stored");
    let sidecar = tierstore_list::sidecar_key(&id_of(listing.key()));
    assert!(rig.farm.object(&sidecar).is_none(), "no sidecar");
    assert!(rig.farm.state().ranged.is_empty(), "no object is read");
    let rows = index
        .unlisted(Duration::from_secs(60), None, 10)
        .expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tree, id_of(listing.key()));
}

/// Catches: a miss path that reads an object twice, or reads one whose cells are recorded: each
/// object the list names is read once, whole, and a second republish of the same tree reads
/// nothing.
#[tokio::test]
async fn the_miss_path_reads_each_object_with_no_cells_once() {
    let (rig, index) = sidecar_rig().await;
    hold_named_objects(&rig);
    let root = one_file_root(&rig).await;
    let kept = rig.kept(&root).await;
    let how = ListingWrite {
        checked: true,
        republish: true,
    };

    rig.writer
        .write_listing_with(&kept, &listed_base(&rig), how, |_| ())
        .await
        .expect("a listing");

    let mut read: BTreeMap<String, u64> = BTreeMap::new();
    for (key, _, len) in &rig.farm.state().ranged {
        *read.entry(key.clone()).or_default() += len;
    }
    let sizes: BTreeMap<String, u64> = [
        ("cas/f/base/p0".to_owned(), 64),
        ("cas/f/w/p9".to_owned(), 4096),
        (
            whole_key("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"),
            5,
        ),
    ]
    .into_iter()
    .collect();
    assert_eq!(read, sizes, "each object read once, whole");
    let cells = index
        .object_cells(&["cas/f/w/p9".to_owned()])
        .expect("cells");
    let digest: [u8; 32] = Sha256::digest(vec![9u8; 4096]).into();
    assert_eq!(cells[0].as_ref().map(|row| row.digests.clone()), Some(vec![digest]));
    let reads = rig.farm.state().ranged.len();

    rig.writer
        .write_listing_with(&kept, &listed_base(&rig), how, |_| ())
        .await
        .expect("a listing");

    assert_eq!(rig.farm.state().ranged.len(), reads, "nothing is read again");
}

/// Catches: a miss path that fails the list when an object it names is gone: the list is still
/// stored and returned, with no sidecar.
#[tokio::test]
async fn a_lost_object_on_the_miss_path_never_fails_the_list() {
    let (rig, _) = sidecar_rig().await;
    let root = one_file_root(&rig).await;
    let kept = rig.kept(&root).await;
    let how = ListingWrite {
        checked: false,
        republish: true,
    };

    let listing = rig
        .writer
        .write_listing_with(&kept, &listed_base(&rig), how, |_| ())
        .await
        .expect("a listing");

    assert!(rig.farm.object(listing.key()).is_some(), "the list is stored");
    let sidecar = tierstore_list::sidecar_key(&id_of(listing.key()));
    assert!(rig.farm.object(&sidecar).is_none(), "no sidecar");
}

/// Catches: a writer that writes a whole object and drops the cells it could have hashed: the
/// cells of an unlocated blob it writes to `cas/l/` are recorded, those of the bytes it wrote.
#[tokio::test]
async fn a_whole_object_the_writer_writes_has_its_cells_recorded() {
    let (rig, index) = sidecar_rig().await;
    let bytes = vec![5u8; (1 << 20) + 3];
    let blob = rig.farm.cas(bytes.clone()).await;
    let root = rig
        .store(&directory(vec![file("big", &blob, false)], vec![], vec![]))
        .await;

    super::listing::list(&rig, &root).await.expect("a listing");

    let key = whole_key(&blob.hash_hex());
    assert!(rig.farm.object(&key).is_some(), "written whole");
    let cells = index.object_cells(&[key]).expect("cells");
    let expected: Vec<[u8; 32]> = bytes
        .chunks(1 << 20)
        .map(|cell| Sha256::digest(cell).into())
        .collect();
    let row = cells[0].clone().expect("recorded");
    assert_eq!((row.size, row.digests), (bytes.len() as u64, expected));
}
