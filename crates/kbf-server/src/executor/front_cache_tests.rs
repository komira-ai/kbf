//! What [`Front`] remembers by digest: the action trees it has written, good for the memo's time
//! on its clock, and the facts of the actions it has read, good for ever.

use bytes::Bytes;
use kbf_meta::{ActionRecord, Role};
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::segments_server::Segments;
use kbf_storage::{Answer, PutStream, ReadStream};

use super::put_dir::{DirPrep, WALL, action_with_input, listed_base, rig_over, rig_with};
use super::*;
use crate::executor::front_cache::Bounded;

/// A storage that counts the questions asked of it about blobs, over another.
struct Counting {
    inner: Arc<dyn Storage>,
    reads: Arc<AtomicUsize>,
}

impl Storage for Counting {
    fn find_missing<'a>(
        &'a self,
        digests: &'a [kbf_types::Digest],
    ) -> Answer<'a, Vec<kbf_types::Digest>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.find_missing(digests)
    }

    fn find_durable<'a>(
        &'a self,
        digests: &'a [kbf_types::Digest],
    ) -> Answer<'a, Vec<kbf_types::Digest>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.find_durable(digests)
    }

    fn read_blob<'a>(&'a self, digest: &'a kbf_types::Digest) -> Answer<'a, Bytes> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.read_blob(digest)
    }

    fn read_range<'a>(
        &'a self,
        d: &'a kbf_types::Digest,
        offset: u64,
        limit: u64,
    ) -> Answer<'a, ReadStream> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.read_range(d, offset, limit)
    }

    fn put(&self, bytes: Bytes) -> Answer<'_, kbf_types::Digest> {
        self.inner.put(bytes)
    }

    fn stream(&self, size: u64) -> Option<Box<dyn PutStream>> {
        self.inner.stream(size)
    }

    fn put_streamed(
        &self,
        size: u64,
        bytes: futures::stream::BoxStream<'static, std::io::Result<Bytes>>,
        claim: futures::future::BoxFuture<'static, Option<kbf_types::Digest>>,
    ) -> Answer<'_, Option<kbf_types::Digest>> {
        self.inner.put_streamed(size, bytes, claim)
    }

    fn segments(&self) -> Arc<dyn Segments> {
        self.inner.segments()
    }

    fn action_result<'a>(
        &'a self,
        a: &'a kbf_types::Digest,
    ) -> Answer<'a, Option<(kbf_types::Digest, ActionResult)>> {
        self.inner.action_result(a)
    }

    fn prepare_action_result<'a>(&'a self, r: &'a ActionResult) -> Answer<'a, ActionRecord> {
        self.inner.prepare_action_result(r)
    }

    fn commit_action_record(
        &self,
        role: Role,
        a: kbf_types::Digest,
        r: ActionRecord,
    ) -> Answer<'_, ()> {
        self.inner.commit_action_record(role, a, r)
    }
}

/// A rig that reads no run's closure and whose CAS reads are counted in the returned counter.
fn counting_rig() -> (Rig, Arc<AtomicUsize>) {
    let reads = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&reads);
    let rig = Rig::reading_through(
        Arc::new(Unprepared),
        |packs, backend| PutDirParts::new(packs, backend, Arc::default()),
        move |inner| Arc::new(Counting { inner, reads }),
    );
    (rig, counter)
}

fn read_count(reads: &AtomicUsize) -> usize {
    reads.load(Ordering::SeqCst)
}

/// The lease of a second run of the same action.
fn second() -> RunId {
    RunId::new(OperationId(7), 4)
}

/// The objects the rig's store has been given.
fn puts(rig: &Rig) -> usize {
    rig.store.puts.load(Ordering::SeqCst)
}

/// Mints the run of `lease` for `action` in `form` and takes its tree: its directory and tree.
async fn run(rig: &Rig, lease: RunId, action: &Digest, form: InputTree) -> (PackKey, String) {
    let dir = rig.front.mint(lease, action, WALL, form).await;
    let dir = dir.expect("a dir");
    let tree = rig.front.tree(lease).await.expect("a tree");
    (dir, tree)
}

/// Asserts that a second mint of one action within the memo's time asks the preparation for the
/// directory alone, writes no tree to the store, and hands out the tree of the first, with a
/// directory of its own that the mint verifies for its lease. Catches: a warm run that walks the
/// closure and writes the tree again, and a hit that reuses the first run's directory.
#[tokio::test]
async fn a_second_run_of_an_action_mints_its_dir_and_reuses_the_tree() {
    let prep = DirPrep::new(None, 7_200);
    let rig = rig_with(Arc::clone(&prep), true);
    let action = action_with_input(&rig, "input").await;
    let (first_dir, first) = run(&rig, lease(), &action, InputTree::Manifests).await;
    let written = puts(&rig);
    assert!(written > 0, "the first tree was written");
    assert_eq!((prep.prepared(), prep.minted()), (1, 0));

    let (dir, tree) = run(&rig, second(), &action, InputTree::Manifests).await;
    assert_eq!(tree, first);
    assert_eq!((prep.prepared(), prep.minted()), (1, 1));
    assert_eq!(
        puts(&rig),
        written,
        "no tree was written for the second run"
    );
    assert_ne!(dir, first_dir);
    let nonce = dir
        .as_str()
        .strip_prefix("cas/b/7-4/")
        .expect("the lease's");
    let issued = IssuedPutDir::new("7-4", nonce).expect("a dir");
    let digest = cas_digest(&action).expect("a digest");
    assert_eq!(rig.mint.verify_put_dir(&issued, &digest), Ok(clock()));
}

/// Asserts that a remembered tree is good for the memo's time on the frontend's clock and no
/// longer: a second before it ends the run is a hit, and at its end the tree is written again.
/// Catches: a tree that never expires although it names pack spans the memo no longer vouches
/// for, and an edge off by one.
#[tokio::test]
async fn a_remembered_tree_is_good_until_the_memo_time_and_then_written_again() {
    let ttl = MEMO_TTL.as_secs();
    let prep = DirPrep::new(None, 7_200);
    let rig = rig_with(Arc::clone(&prep), true);
    let action = action_with_input(&rig, "input").await;
    let (_, first) = run(&rig, lease(), &action, InputTree::Manifests).await;

    CLOCK.with(|c| c.set(NOW + ttl - 1));
    let (_, tree) = run(&rig, second(), &action, InputTree::Manifests).await;
    assert_eq!(tree, first);
    assert_eq!((prep.prepared(), prep.minted()), (1, 1));

    CLOCK.with(|c| c.set(NOW + ttl));
    let (_, tree) = run(
        &rig,
        RunId::new(OperationId(7), 5),
        &action,
        InputTree::Manifests,
    )
    .await;
    assert_eq!(tree, first, "the same tree, written again");
    assert_eq!((prep.prepared(), prep.minted()), (2, 1));
}

/// Asserts that the form of the tree is part of what is remembered: the manifests and the tree
/// list of one action are two entries, each written once and then a hit. Catches: a run asked
/// for a tree list handed the manifests' digest (or the other way about) by a hit.
#[tokio::test]
async fn the_manifests_and_the_tree_list_of_one_action_are_two_entries() {
    let prep = DirPrep::new(None, 7_200);
    let rig = rig_over(Arc::clone(&prep), Some(listed_base()));
    let action = action_with_input(&rig, "input").await;
    let (_, manifests) = run(&rig, lease(), &action, InputTree::Manifests).await;
    let (_, listed) = run(&rig, second(), &action, InputTree::Provider).await;
    assert!(!manifests.starts_with("cas/l/"), "{manifests}");
    assert!(listed.starts_with("cas/l/"), "{listed}");
    assert_eq!((prep.prepared(), prep.minted()), (2, 0));

    let (_, again) = run(
        &rig,
        RunId::new(OperationId(7), 5),
        &action,
        InputTree::Manifests,
    )
    .await;
    assert_eq!(again, manifests);
    let (_, again) = run(
        &rig,
        RunId::new(OperationId(7), 6),
        &action,
        InputTree::Provider,
    )
    .await;
    assert_eq!(again, listed);
    assert_eq!((prep.prepared(), prep.minted()), (2, 2));
}

/// Asserts that a tree that could not be written is not remembered: an input root the writer
/// refuses, and a store that cannot answer, each leave the next run of the action to prepare and
/// write its tree, and the run that then succeeds is remembered. Catches: a failure cached as
/// if it were a tree, and a refused run parked with no tree.
#[tokio::test]
async fn a_tree_that_could_not_be_written_is_not_remembered() {
    let prep = DirPrep::new(None, 7_200);
    let rig = rig_with(Arc::clone(&prep), true);
    let reserved = action_with_input(&rig, ".kbf-out").await;
    for lease in [lease(), second()] {
        let refused = rig
            .front
            .mint(lease, &reserved, WALL, InputTree::Manifests)
            .await;
        assert!(
            matches!(&refused, Err(MintError::Refused(why)) if why.contains(".kbf-out")),
            "{refused:?}"
        );
        assert!(rig.front.tree(lease).await.is_err());
    }
    assert_eq!((prep.prepared(), prep.minted()), (2, 0));

    let action = action_with_input(&rig, "input").await;
    rig.store.fail_reads.store(true, Ordering::SeqCst);
    let down = rig
        .front
        .mint(lease(), &action, WALL, InputTree::Manifests)
        .await;
    assert!(matches!(down, Err(MintError::Unavailable(_))), "{down:?}");
    rig.store.fail_reads.store(false, Ordering::SeqCst);
    assert_eq!((prep.prepared(), prep.minted()), (3, 0));

    run(&rig, second(), &action, InputTree::Manifests).await;
    assert_eq!((prep.prepared(), prep.minted()), (4, 0));
    run(
        &rig,
        RunId::new(OperationId(7), 5),
        &action,
        InputTree::Manifests,
    )
    .await;
    assert_eq!((prep.prepared(), prep.minted()), (4, 1));
}

/// Asserts that a hit whose directory the preparation refuses settles as a miss would, by whose
/// fault it is, parks nothing, writes nothing and keeps the tree remembered for the next run.
/// Catches: a refused hit that hands out the cached tree anyway, that is taken for the farm's
/// fault when it is the caller's, or that drops the entry.
#[tokio::test]
async fn a_hit_whose_dir_is_refused_settles_as_a_miss_does_and_forgets_nothing() {
    let prep = DirPrep::new(None, 7_200);
    let rig = rig_with(Arc::clone(&prep), true);
    let action = action_with_input(&rig, "input").await;
    let (_, first) = run(&rig, lease(), &action, InputTree::Manifests).await;
    let written = puts(&rig);

    prep.refuses_mints(Some(PrepareError::Refused("over budget".to_owned())));
    let refused = rig
        .front
        .mint(second(), &action, WALL, InputTree::Manifests)
        .await;
    assert_eq!(refused, Err(MintError::Refused("over budget".to_owned())));
    prep.refuses_mints(Some(PrepareError::Unavailable("the store".to_owned())));
    let down = rig
        .front
        .mint(second(), &action, WALL, InputTree::Manifests)
        .await;
    assert_eq!(down, Err(MintError::Unavailable("the store".to_owned())));
    assert!(rig.front.tree(second()).await.is_err(), "nothing is parked");
    assert_eq!((prep.prepared(), prep.minted()), (1, 2));
    assert_eq!(puts(&rig), written);

    prep.refuses_mints(None);
    let (_, tree) = run(&rig, second(), &action, InputTree::Manifests).await;
    assert_eq!(tree, first);
    assert_eq!((prep.prepared(), prep.minted()), (1, 3));
}

/// Asserts that a hit is judged by the keys of the directory it mints as a miss is: keys that
/// end a second before the run's wall does are refused with the miss's message and park
/// nothing, and keys that end with it are taken. Catches: a cached tree that skips the check
/// that the run outlives its directory.
#[tokio::test]
async fn a_hit_whose_dir_ends_before_the_run_does_is_refused() {
    let prep = DirPrep::new(None, 7_200);
    let rig = rig_with(Arc::clone(&prep), true);
    let action = action_with_input(&rig, "input").await;
    run(&rig, lease(), &action, InputTree::Manifests).await;

    prep.mints_good_for(WALL.as_secs() - 1);
    let refused = rig
        .front
        .mint(second(), &action, WALL, InputTree::Manifests)
        .await;
    assert_eq!(
        refused,
        Err(MintError::Refused(
            "the put directory's keys end before the run's wall time does".to_owned()
        ))
    );
    assert!(rig.front.tree(second()).await.is_err(), "nothing is parked");

    prep.mints_good_for(WALL.as_secs());
    run(&rig, second(), &action, InputTree::Manifests).await;
    assert_eq!(prep.prepared(), 1);
}

/// Asserts that a digest that is no digest is refused as the caller's before anything is
/// prepared, remembered or looked up.
#[tokio::test]
async fn an_action_digest_that_is_not_one_is_refused_and_not_prepared() {
    let prep = DirPrep::new(None, 7_200);
    let rig = rig_with(Arc::clone(&prep), true);
    let not_a_digest = reapi::Digest {
        hash: "xyz".to_owned(),
        size_bytes: 1,
    };
    let refused = rig
        .front
        .mint(lease(), &not_a_digest, WALL, InputTree::Manifests)
        .await;
    assert!(matches!(refused, Err(MintError::Refused(_))), "{refused:?}");
    assert_eq!((prep.prepared(), prep.minted()), (0, 0));
}

/// Asserts that the bound drops the oldest tree: with room for one, a second action's run
/// pushes the first out, so its next run is written again, while the second's is a hit.
#[tokio::test]
async fn at_the_bound_the_oldest_tree_is_dropped() {
    let prep = DirPrep::new(None, 7_200);
    let rig = rig_with(Arc::clone(&prep), true);
    let rig = Rig {
        front: rig.front.with_bounds(1, 1),
        ..rig
    };
    let a = action_with_input(&rig, "a").await;
    let b = action_with_input(&rig, "b").await;
    run(&rig, lease(), &a, InputTree::Manifests).await;
    run(&rig, lease(), &b, InputTree::Manifests).await;
    assert_eq!((prep.prepared(), prep.minted()), (2, 0));
    run(&rig, second(), &b, InputTree::Manifests).await;
    assert_eq!((prep.prepared(), prep.minted()), (2, 1), "b is remembered");
    run(&rig, second(), &a, InputTree::Manifests).await;
    assert_eq!((prep.prepared(), prep.minted()), (3, 1), "a was dropped");
}

/// Asserts that the facts of an action are read from the CAS once: a second question about the
/// same Action returns the same facts and reads no blob. Catches: a warm prepare that reads the
/// Action and the Command again.
#[tokio::test]
async fn the_facts_of_an_action_are_read_once() {
    let (rig, reads) = counting_rig();
    let action = rig.action(|_| {}, |_| {}).await;
    let first = rig.front.action(&action).await.expect("facts");
    let after_first = read_count(&reads);
    assert!(after_first > 0, "the first question reads the CAS");
    let again = rig.front.action(&action).await.expect("facts");
    assert_eq!(again, first);
    assert_eq!(read_count(&reads), after_first);
}

/// Asserts that a failed read is not remembered: an Action the CAS does not hold, asked after
/// twice, reads the CAS each time.
#[tokio::test]
async fn a_failed_read_of_an_action_is_read_again() {
    let (rig, reads) = counting_rig();
    let unheld = rig.front.action(&action_digest()).await;
    assert!(matches!(unheld, Err(InputsError::Unavailable(_))));
    let after_first = read_count(&reads);
    assert!(after_first > 0);
    let unheld = rig.front.action(&action_digest()).await;
    assert!(matches!(unheld, Err(InputsError::Unavailable(_))));
    assert!(
        read_count(&reads) > after_first,
        "the second question read again"
    );
}

/// Asserts that the bound drops the oldest facts: with room for one, a second action's facts
/// push the first out, so asking after the first again reads the CAS.
#[tokio::test]
async fn at_the_bound_the_oldest_facts_are_dropped() {
    let (rig, reads) = counting_rig();
    let rig = Rig {
        front: rig.front.with_bounds(1, 1),
        ..rig
    };
    let a = rig.action(|a| a.timeout = Some(seconds(1)), |_| {}).await;
    let b = rig.action(|a| a.timeout = Some(seconds(2)), |_| {}).await;
    rig.front.action(&a).await.expect("facts");
    rig.front.action(&b).await.expect("facts");
    let before = read_count(&reads);
    rig.front.action(&b).await.expect("facts");
    assert_eq!(read_count(&reads), before, "b is remembered");
    rig.front.action(&a).await.expect("facts");
    assert!(read_count(&reads) > before, "a was dropped");
}

/// Asserts that a full map drops the entry remembered first, and that remembering a key again
/// makes it the newest rather than keeping two.
#[test]
fn a_full_map_drops_the_entry_remembered_first() {
    let mut map = Bounded::new(2);
    map.put(1, "one");
    map.put(2, "two");
    map.put(3, "three");
    assert_eq!(
        (map.get(&1), map.get(&2), map.get(&3)),
        (None, Some("two"), Some("three"))
    );

    let mut map = Bounded::new(2);
    map.put(1, "one");
    map.put(2, "two");
    map.put(1, "uno");
    map.put(3, "three");
    assert_eq!(
        (map.get(&1), map.get(&2), map.get(&3)),
        (Some("uno"), None, Some("three"))
    );
}

/// Asserts that remembering a live key again replaces its value and does not grow the map: at a
/// bound of one the newer value is held, and the next key then drops it.
#[test]
fn remembering_a_key_again_replaces_its_value_in_place() {
    let mut map = Bounded::new(1);
    map.put(1, "one");
    map.put(1, "uno");
    assert_eq!(map.get(&1), Some("uno"));
    map.put(2, "two");
    assert_eq!((map.get(&1), map.get(&2)), (None, Some("two")));
}

/// Asserts that a key forgotten is gone and its value handed back, that forgetting a key the map
/// does not hold changes nothing, and that a forgotten key no longer counts toward the bound: the
/// entries remembered after it are kept.
#[test]
fn a_forgotten_key_is_gone_and_frees_its_place() {
    let mut map = Bounded::new(2);
    map.put(1, "one");
    map.put(2, "two");
    assert_eq!(map.remove(&1), Some("one"));
    assert_eq!(map.remove(&1), None);
    assert_eq!(map.remove(&9), None);
    map.put(3, "three");
    assert_eq!(
        (map.get(&1), map.get(&2), map.get(&3)),
        (None, Some("two"), Some("three"))
    );
}
